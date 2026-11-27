use egui::{Color32, ColorImage, Context, Pos2, Rect, TextureHandle, TextureOptions, Vec2};
use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Networking::WinHttp::{
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTPS, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS, WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::core::{PCWSTR, PWSTR};

pub const DEFAULT_GIF_BYTES: &[u8] = include_bytes!("../assets/default.gif");
pub const KAINE_GIF_BYTES: &[u8] = DEFAULT_GIF_BYTES;
pub const MAX_GIF_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_GIF_FRAMES: usize = 400;
const TARGET_WINDOW_AREA: f32 = 300.0 * 228.0;

static NEXT_GIF_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Adaptive UI accent/border/text palette derived from the active GIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifPalette {
    pub accent: Color32,
    pub accent_bright: Color32,
    pub silver: Color32,
    pub border: Color32,
    pub corner: Color32,
}

impl Default for GifPalette {
    fn default() -> Self {
        Self::nier_default()
    }
}

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;
    let h = if delta < 1e-5 {
        0.0
    } else if (max - rf).abs() < 1e-5 {
        60.0 * (((gf - bf) / delta) % 6.0)
    } else if (max - gf).abs() < 1e-5 {
        60.0 * (((bf - rf) / delta) + 2.0)
    } else {
        60.0 * (((rf - gf) / delta) + 4.0)
    };
    let h_norm = if h < 0.0 { h + 360.0 } else { h };
    let s = if max < 1e-5 { 0.0 } else { delta / max };
    (h_norm, s, max)
}

fn hsv_to_color32(h: f32, s: f32, v: f32) -> Color32 {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (rf, gf, bf) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((rf + m) * 255.0).round() as u8,
        ((gf + m) * 255.0).round() as u8,
        ((bf + m) * 255.0).round() as u8,
    )
}

impl GifPalette {
    /// Original handcrafted NieR Replicant / Kaine palette.
    pub fn nier_default() -> Self {
        Self {
            accent: Color32::from_rgb(126, 174, 196),        // #7eaec4
            accent_bright: Color32::from_rgb(164, 200, 217), // #a4c8d9
            silver: Color32::from_rgb(143, 162, 181),        // #8fa2b5
            border: Color32::from_rgb(54, 65, 78),           // #36414e
            corner: Color32::from_rgb(74, 91, 109),          // #4a5b6d
        }
    }

    /// Extracts an adaptive, legible dark-UI accent palette from the first frame of a custom GIF.
    pub fn from_image(img: &ColorImage) -> Self {
        if img.pixels.is_empty() {
            return Self::nier_default();
        }
        let mut buckets = [0.0f32; 24];
        let mut sat_sum = [0.0f32; 24];
        let stride = (img.pixels.len() / 1600).max(1);

        for px in img.pixels.iter().step_by(stride) {
            if px.a() < 64 {
                continue;
            }
            let (h, s, v) = rgb_to_hsv(px.r(), px.g(), px.b());
            if s >= 0.16 && (0.15..=0.96).contains(&v) {
                let bin = ((h / 15.0).floor() as usize).min(23);
                let weight = s * (0.4 + 0.6 * v);
                buckets[bin] += weight;
                sat_sum[bin] += s * weight;
            }
        }

        let (best_bin, &best_weight) = buckets
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or((0, &0.0));

        if best_weight <= 0.5 {
            // Monochrome/neutral GIF: clean lunar-neutral palette
            return Self {
                accent: Color32::from_rgb(185, 196, 208),
                accent_bright: Color32::from_rgb(220, 228, 236),
                silver: Color32::from_rgb(176, 184, 194),
                border: Color32::from_rgb(66, 72, 80),
                corner: Color32::from_rgb(94, 102, 112),
            };
        }

        let hue = (best_bin as f32) * 15.0 + 7.5;
        let avg_sat = (sat_sum[best_bin] / best_weight).clamp(0.28, 0.56);

        Self {
            accent: hsv_to_color32(hue, avg_sat, 0.78),
            accent_bright: hsv_to_color32(hue, (avg_sat * 0.75).clamp(0.18, 0.45), 0.88),
            silver: hsv_to_color32(hue, (avg_sat * 0.42).clamp(0.10, 0.25), 0.78),
            border: hsv_to_color32(hue, (avg_sat * 0.60).clamp(0.15, 0.35), 0.32),
            corner: hsv_to_color32(hue, (avg_sat * 0.55).clamp(0.15, 0.35), 0.45),
        }
    }
}

/// Computes an aspect-preserving companion window size `[width, height]` matching a ~`300x228` area.
pub fn compute_window_size_for_gif(dims: (usize, usize)) -> [f32; 2] {
    if dims.0 == 0 || dims.1 == 0 {
        return [300.0, 228.0];
    }
    let aspect = (dims.0 as f32) / (dims.1 as f32);
    let mut w = (TARGET_WINDOW_AREA * aspect).sqrt();
    let mut h = TARGET_WINDOW_AREA / w;

    if w > 380.0 {
        h *= 380.0 / w;
        w = 380.0;
    }
    if h > 360.0 {
        w *= 360.0 / h;
        h = 360.0;
    }

    [
        w.round().clamp(200.0, 380.0),
        h.round().clamp(160.0, 360.0),
    ]
}

pub struct GifPlayer {
    frames: Vec<(ColorImage, Duration)>,
    textures: Vec<TextureHandle>,
    current_index: usize,
    last_frame_time: Instant,
    dimensions: (usize, usize),
    generation: u64,
    palette: GifPalette,
}

impl GifPlayer {
    /// Decodes raw GIF bytes and pre-caches frame images into memory (capped at `MAX_GIF_FRAMES`).
    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if !is_gif_magic(bytes) {
            return Err("Not a valid GIF image".to_string());
        }
        let decoder =
            GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("GIF decode error: {}", e))?;

        let mut frames = Vec::new();
        let mut first_dims = (0, 0);

        for (i, frame_res) in decoder.into_frames().take(MAX_GIF_FRAMES).enumerate() {
            let frame = frame_res.map_err(|e| format!("Collect frame error: {}", e))?;
            let (num, den) = frame.delay().numer_denom_ms();
            let ms = if den > 0 {
                (num as f64 / den as f64).round() as u64
            } else {
                100
            };
            let delay = Duration::from_millis(ms.max(16));
            let buffer = frame.into_buffer();
            let size = [buffer.width() as usize, buffer.height() as usize];
            if i == 0 {
                first_dims = (size[0], size[1]);
            }
            let pixels = buffer.into_raw();
            let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            frames.push((color_image, delay));
        }

        if frames.is_empty() {
            return Err("GIF contains no frames".to_string());
        }

        let palette = GifPalette::from_image(&frames[0].0);

        Ok(Self {
            frames,
            textures: Vec::new(),
            current_index: 0,
            last_frame_time: Instant::now(),
            dimensions: first_dims,
            generation: NEXT_GIF_GENERATION.fetch_add(1, Ordering::Relaxed),
            palette,
        })
    }

    /// Loads custom saved GIF if present, otherwise falls back to built-in Kaine GIF.
    pub fn load_default() -> Self {
        let mut custom_candidates = vec![get_custom_gif_path()];
        custom_candidates.push(PathBuf::from("data/assets/custom.gif"));

        for p in &custom_candidates {
            if p.exists()
                && let Ok(bytes) = std::fs::read(p)
                && let Ok(player) = Self::load_from_bytes(&bytes)
            {
                return player;
            }
        }

        Self::load_builtin_default()
    }

    /// Loads `default.gif` from disk or embedded bytes.
    pub fn load_builtin_default() -> Self {
        let mut candidates: Vec<PathBuf> = Vec::new();

        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            candidates.push(exe_dir.join("data").join("assets").join("default.gif"));
            candidates.push(exe_dir.join("assets").join("default.gif"));
        }
        candidates.push(PathBuf::from("data/assets/default.gif"));
        candidates.push(PathBuf::from("assets/default.gif"));

        for p in &candidates {
            if p.exists()
                && let Ok(bytes) = std::fs::read(p)
                && let Ok(player) = Self::load_from_bytes(&bytes)
            {
                return player;
            }
        }

        Self::new_embedded()
    }

    /// Deletes any persisted custom GIF and restores the built-in default GIF.
    pub fn reset_custom_gif() -> Self {
        let _ = std::fs::remove_file(get_custom_gif_path());
        let _ = std::fs::remove_file("data/assets/custom.gif");
        Self::load_builtin_default()
    }

    /// Loads the embedded default.gif compiled into the binary.
    pub fn new_embedded() -> Self {
        Self::load_from_bytes(DEFAULT_GIF_BYTES).expect("Failed to load embedded default.gif")
    }

    pub fn palette(&self) -> GifPalette {
        self.palette
    }

    pub fn target_window_size(&self) -> [f32; 2] {
        compute_window_size_for_gif(self.dimensions)
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn dimensions(&self) -> (usize, usize) {
        self.dimensions
    }

    pub fn size(&self) -> Vec2 {
        Vec2::new(self.dimensions.0 as f32, self.dimensions.1 as f32)
    }

    /// Computes center-cropped cover UV coordinates so any GIF aspect ratio fills `target_size` without stretching.
    pub fn compute_cover_uv(&self, target_size: Vec2) -> Rect {
        if self.dimensions.0 == 0
            || self.dimensions.1 == 0
            || target_size.x <= 0.0
            || target_size.y <= 0.0
        {
            return Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        }

        let gif_aspect = (self.dimensions.0 as f32) / (self.dimensions.1 as f32);
        let target_aspect = target_size.x / target_size.y;

        if (gif_aspect - target_aspect).abs() < 0.01 {
            return Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        }

        if gif_aspect > target_aspect {
            let keep = (target_aspect / gif_aspect).clamp(0.05, 1.0);
            let pad = (1.0 - keep) * 0.5;
            Rect::from_min_max(Pos2::new(pad, 0.0), Pos2::new(1.0 - pad, 1.0))
        } else {
            let keep = (gif_aspect / target_aspect).clamp(0.05, 1.0);
            let pad = (1.0 - keep) * 0.5;
            Rect::from_min_max(Pos2::new(0.0, pad), Pos2::new(1.0, 1.0 - pad))
        }
    }

    /// Pre-caches all decoded frame buffers into GPU texture handles.
    pub fn prepare_textures(&mut self, ctx: &Context) {
        if self.textures.is_empty() && !self.frames.is_empty() {
            self.textures = self
                .frames
                .iter()
                .enumerate()
                .map(|(i, (img, _))| {
                    ctx.load_texture(
                        format!("freak_gif_{}_{}", self.generation, i),
                        img.clone(),
                        TextureOptions::LINEAR,
                    )
                })
                .collect();
        }
    }

    /// Returns the active frame texture handle and requests a scheduled repaint.
    pub fn update_and_get_texture(&mut self, ctx: &Context) -> Option<&TextureHandle> {
        if self.frames.is_empty() {
            return None;
        }

        self.prepare_textures(ctx);

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame_time);
        let mut current_delay = self.frames[self.current_index].1;

        if elapsed >= current_delay {
            let mut remaining_elapsed = elapsed;
            while remaining_elapsed >= current_delay {
                remaining_elapsed -= current_delay;
                self.current_index = (self.current_index + 1) % self.frames.len();
                current_delay = self.frames[self.current_index].1;
            }
            self.last_frame_time = now.checked_sub(remaining_elapsed).unwrap_or(now);
        }

        let current_elapsed = now.duration_since(self.last_frame_time);
        let remaining = current_delay.saturating_sub(current_elapsed);
        ctx.request_repaint_after(remaining);

        self.textures.get(self.current_index)
    }
}

pub fn get_custom_gif_path() -> PathBuf {
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        return exe_dir.join("data").join("assets").join("custom.gif");
    }
    PathBuf::from("data/assets/custom.gif")
}

pub fn is_gif_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
}

fn unescape_html_url(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace(r"\/", "/")
        .replace(r"\u002F", "/")
        .replace(r"\u002f", "/")
        .replace(r"\u0026", "&")
}

fn extract_attr_value(tag: &str, attr_name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=", attr_name.to_ascii_lowercase());
    let idx = lower.find(&needle)?;
    let rest = &tag[idx + needle.len()..];
    let trimmed = rest.trim_start();
    let quote = trimmed.chars().next()?;
    if quote == '"' || quote == '\'' {
        let after = &trimmed[1..];
        let end = after.find(quote)?;
        Some(after[..end].to_string())
    } else {
        let end = trimmed
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(trimmed.len());
        Some(trimmed[..end].to_string())
    }
}

/// Extracts a direct `.gif` URL from share-page HTML (e.g. Tenor, Giphy, or OpenGraph pages).
pub fn extract_gif_url_from_html(html: &str) -> Option<String> {
    // 1. Check <meta> OpenGraph / Twitter image tags
    let lower = html.to_ascii_lowercase();
    let mut pos = 0usize;
    while let Some(rel_start) = lower[pos..].find("<meta") {
        let start = pos + rel_start;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e + 1)
            .unwrap_or(html.len());
        let tag = &html[start..end];
        let tag_lower = &lower[start..end];
        if (tag_lower.contains("og:image") || tag_lower.contains("twitter:image"))
            && !tag_lower.contains("og:image:width")
            && !tag_lower.contains("og:image:height")
            && let Some(content) = extract_attr_value(tag, "content")
        {
            let url = unescape_html_url(&content);
            let u_low = url.to_ascii_lowercase();
            if (u_low.starts_with("https://") || u_low.starts_with("http://"))
                && u_low.contains(".gif")
            {
                return Some(url);
            }
        }
        pos = end;
    }

    // 2. Check Tenor JSON-LD "contentUrl":"https://...gif..."
    if let Some(cu_pos) = html.find("\"contentUrl\"") {
        let after = &html[cu_pos + 12..];
        if let Some(q1) = after.find('"') {
            let rest = &after[q1 + 1..];
            if let Some(q2) = rest.find('"') {
                let url = unescape_html_url(&rest[..q2]);
                let u_low = url.to_ascii_lowercase();
                if (u_low.starts_with("https://") || u_low.starts_with("http://"))
                    && u_low.contains(".gif")
                {
                    return Some(url);
                }
            }
        }
    }

    // 3. General scan for https://...gif URLs
    let mut scan = 0usize;
    while let Some(rel) = lower[scan..].find("http") {
        let start = scan + rel;
        let slice = &html[start..];
        if !(slice.starts_with("https://") || slice.starts_with("http://")) {
            scan = start + 4;
            continue;
        }
        let end = slice
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | '\\')
            })
            .unwrap_or(slice.len());
        let candidate = unescape_html_url(&slice[..end]);
        let c_low = candidate.to_ascii_lowercase();
        if c_low.contains(".gif")
            && !c_low.contains("favicon")
            && !c_low.contains("spacer")
            && !c_low.contains("logo")
        {
            return Some(candidate);
        }
        scan = start + end.max(4);
    }

    None
}

struct WinHttpHandle(*mut core::ffi::c_void);

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// Fetches raw bytes from an `http://` or `https://` URL using Windows WinHTTP.
pub fn fetch_url_bytes(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("URL must start with http:// or https://".to_string());
    }

    let mut url_utf16: Vec<u16> = url.encode_utf16().collect();
    url_utf16.push(0);

    let mut host_buf = vec![0u16; 256];
    let mut path_buf = vec![0u16; 2048];
    let mut extra_buf = vec![0u16; 2048];

    let mut comps = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: PWSTR(host_buf.as_mut_ptr()),
        dwHostNameLength: host_buf.len() as u32,
        lpszUrlPath: PWSTR(path_buf.as_mut_ptr()),
        dwUrlPathLength: path_buf.len() as u32,
        lpszExtraInfo: PWSTR(extra_buf.as_mut_ptr()),
        dwExtraInfoLength: extra_buf.len() as u32,
        ..Default::default()
    };

    unsafe {
        WinHttpCrackUrl(&url_utf16[..url_utf16.len() - 1], 0, &mut comps)
            .map_err(|e| format!("Invalid URL: {}", e))?;

        let host = String::from_utf16_lossy(&host_buf[..comps.dwHostNameLength as usize]);
        let path_part = String::from_utf16_lossy(&path_buf[..comps.dwUrlPathLength as usize]);
        let extra_part = String::from_utf16_lossy(&extra_buf[..comps.dwExtraInfoLength as usize]);
        let full_path = if path_part.is_empty() {
            format!("/{}", extra_part)
        } else {
            format!("{}{}", path_part, extra_part)
        };

        let agent: Vec<u16> =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) FramePacerCompanion/0.3\0"
                .encode_utf16()
                .collect();
        let mut session_raw = WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session_raw.is_null() {
            session_raw = WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            );
        }
        if session_raw.is_null() {
            return Err("Failed to initialize WinHTTP session".to_string());
        }
        let session = WinHttpHandle(session_raw);
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 15_000);

        let mut host_utf16: Vec<u16> = host.encode_utf16().collect();
        host_utf16.push(0);
        let conn_raw = WinHttpConnect(session.0, PCWSTR(host_utf16.as_ptr()), comps.nPort, 0);
        if conn_raw.is_null() {
            return Err(format!("Failed to connect to {}", host));
        }
        let conn = WinHttpHandle(conn_raw);

        let verb: Vec<u16> = "GET\0".encode_utf16().collect();
        let mut object_utf16: Vec<u16> = full_path.encode_utf16().collect();
        object_utf16.push(0);
        let flags = if comps.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS {
            WINHTTP_FLAG_SECURE
        } else {
            Default::default()
        };
        let req_raw = WinHttpOpenRequest(
            conn.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(object_utf16.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        );
        if req_raw.is_null() {
            return Err("Failed to create HTTP request".to_string());
        }
        let req = WinHttpHandle(req_raw);

        let redirect_policy = WINHTTP_OPTION_REDIRECT_POLICY_ALWAYS.to_ne_bytes();
        let _ = WinHttpSetOption(
            Some(req.0),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&redirect_policy),
        );

        WinHttpSendRequest(req.0, None, None, 0, 0, 0)
            .map_err(|e| format!("HTTP request failed: {}", e))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut())
            .map_err(|e| format!("HTTP response failed: {}", e))?;

        let mut out = Vec::new();
        loop {
            let mut avail = 0u32;
            WinHttpQueryDataAvailable(req.0, &mut avail)
                .map_err(|e| format!("Download error: {}", e))?;
            if avail == 0 {
                break;
            }
            if out.len().saturating_add(avail as usize) > max_bytes {
                return Err("GIF exceeds 20 MB size limit".to_string());
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read = 0u32;
            WinHttpReadData(
                req.0,
                chunk.as_mut_ptr() as *mut core::ffi::c_void,
                avail,
                &mut read,
            )
            .map_err(|e| format!("Read error: {}", e))?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read as usize]);
        }

        Ok(out)
    }
}

/// Loads a custom GIF from a direct URL, share page URL (Tenor/Giphy), or local file path,
/// and persists it to `data/assets/custom.gif`.
pub fn load_custom_gif_source(source: &str) -> Result<GifPlayer, String> {
    let trimmed = source
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();
    if trimmed.is_empty() {
        return Err("Please enter a GIF URL or file path".to_string());
    }

    let lower = trimmed.to_ascii_lowercase();
    let gif_bytes = if lower.starts_with("http://") || lower.starts_with("https://") {
        let initial = fetch_url_bytes(trimmed, MAX_GIF_BYTES)?;
        if is_gif_magic(&initial) {
            initial
        } else {
            let html = std::str::from_utf8(&initial)
                .map_err(|_| "Link did not return a GIF image".to_string())?;
            let resolved = extract_gif_url_from_html(html)
                .ok_or_else(|| "Could not find a .gif inside that link".to_string())?;
            let fetched = fetch_url_bytes(&resolved, MAX_GIF_BYTES)?;
            if !is_gif_magic(&fetched) {
                return Err("Resolved link did not return a valid GIF".to_string());
            }
            fetched
        }
    } else {
        let path = PathBuf::from(trimmed);
        let meta = std::fs::metadata(&path).map_err(|_| "GIF file not found".to_string())?;
        if meta.len() as usize > MAX_GIF_BYTES {
            return Err("GIF file exceeds 20 MB limit".to_string());
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("Failed to read file: {}", e))?;
        if !is_gif_magic(&bytes) {
            return Err("Selected file is not a valid GIF".to_string());
        }
        bytes
    };

    let player = GifPlayer::load_from_bytes(&gif_bytes)?;

    let save_path = get_custom_gif_path();
    if let Some(parent) = save_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&save_path, &gif_bytes) {
        crate::logger::log_warn(format!("Could not save custom GIF to disk: {}", e));
    } else {
        crate::logger::log_info(format!(
            "[GIF] Custom GIF loaded ({} frames, {}x{}) -> {}",
            player.frame_count(),
            player.dimensions().0,
            player.dimensions().1,
            save_path.display()
        ));
    }

    Ok(player)
}

/// Opens a native Windows file picker filtered to `.gif` files.
pub fn pick_gif_file_dialog() -> Option<PathBuf> {
    let filter: Vec<u16> = "GIF Images (*.gif)\0*.gif\0All Files (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Select Custom GIF\0".encode_utf16().collect();
    let mut file_buf = vec![0u16; 1024];

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: file_buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };

    let ok = unsafe { GetOpenFileNameW(&mut ofn).as_bool() };
    if !ok {
        return None;
    }
    let end = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    let path_str = String::from_utf16_lossy(&file_buf[..end]);
    if path_str.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_gif_decoding() {
        let player = GifPlayer::new_embedded();
        assert_eq!(player.frame_count(), 108);
        assert_eq!(player.dimensions(), (498, 498));
        assert_eq!(player.size(), Vec2::new(498.0, 498.0));
    }

    #[test]
    fn test_load_default() {
        let player = GifPlayer::load_builtin_default();
        assert_eq!(player.frame_count(), 108);
        assert_eq!(player.dimensions(), (498, 498));
    }

    #[test]
    fn test_is_gif_magic() {
        assert!(is_gif_magic(b"GIF89a..."));
        assert!(is_gif_magic(b"GIF87a..."));
        assert!(!is_gif_magic(b"\x89PNG\r\n"));
        assert!(!is_gif_magic(b"<html>"));
        assert!(!is_gif_magic(b"GIF"));
    }

    #[test]
    fn test_extract_gif_url_from_tenor_and_giphy_html() {
        let og_html = r#"<html><head><meta property="og:image" content="https://media.tenor.com/abc123/nier.gif?v=1&amp;size=full"></head></html>"#;
        assert_eq!(
            extract_gif_url_from_html(og_html).as_deref(),
            Some("https://media.tenor.com/abc123/nier.gif?v=1&size=full")
        );

        let tenor_ld = r#"<script>{"contentUrl":"https:\/\/media1.tenor.com\/m\/xyz\/anime.gif"}</script>"#;
        assert_eq!(
            extract_gif_url_from_html(tenor_ld).as_deref(),
            Some("https://media1.tenor.com/m/xyz/anime.gif")
        );

        let giphy_html = r#"<html><body><img src="https://media.giphy.com/media/12345/giphy.gif"></body></html>"#;
        assert_eq!(
            extract_gif_url_from_html(giphy_html).as_deref(),
            Some("https://media.giphy.com/media/12345/giphy.gif")
        );
    }

    #[test]
    fn test_compute_cover_uv_preserves_aspect_ratio() {
        let mut player = GifPlayer::new_embedded();
        // Exact match (~1.314 vs 300/228 = 1.315) -> full UV
        let uv_exact = player.compute_cover_uv(Vec2::new(300.0, 228.0));
        assert!((uv_exact.min.x - 0.0).abs() < 0.01);
        assert!((uv_exact.max.x - 1.0).abs() < 0.01);

        // Wide 16:9 GIF in 300x228 -> crops horizontally (min.x > 0, max.x < 1)
        player.dimensions = (1920, 1080);
        let uv_wide = player.compute_cover_uv(Vec2::new(300.0, 228.0));
        assert!(uv_wide.min.x > 0.1 && uv_wide.max.x < 0.9);
        assert_eq!(uv_wide.min.y, 0.0);
        assert_eq!(uv_wide.max.y, 1.0);

        // Tall portrait GIF in 300x228 -> crops vertically (min.y > 0, max.y < 1)
        player.dimensions = (300, 600);
        let uv_tall = player.compute_cover_uv(Vec2::new(300.0, 228.0));
        assert_eq!(uv_tall.min.x, 0.0);
        assert_eq!(uv_tall.max.x, 1.0);
        assert!(uv_tall.min.y > 0.2 && uv_tall.max.y < 0.8);
    }

    #[test]
    fn test_compute_window_size_for_various_aspects() {
        assert_eq!(compute_window_size_for_gif((498, 379)), [300.0, 228.0]);
        assert_eq!(compute_window_size_for_gif((404, 498)), [236.0, 290.0]);
        assert_eq!(compute_window_size_for_gif((500, 500)), [262.0, 262.0]);
        assert_eq!(compute_window_size_for_gif((1920, 1080)), [349.0, 196.0]);

        let ultra_wide = compute_window_size_for_gif((2000, 200));
        assert!(ultra_wide[0] <= 380.0 && ultra_wide[1] >= 160.0);

        let ultra_tall = compute_window_size_for_gif((200, 2000));
        assert!(ultra_tall[0] >= 200.0 && ultra_tall[1] <= 360.0);
    }

    #[test]
    fn test_palette_extraction_warm_and_monochrome() {
        let warm_pixels = vec![Color32::from_rgb(220, 80, 40); 100];
        let warm_img = ColorImage {
            size: [10, 10],
            pixels: warm_pixels,
        };
        let warm_pal = GifPalette::from_image(&warm_img);
        assert!(warm_pal.accent.r() > warm_pal.accent.b());

        let gray_pixels = vec![Color32::from_rgb(128, 128, 128); 100];
        let gray_img = ColorImage {
            size: [10, 10],
            pixels: gray_pixels,
        };
        let gray_pal = GifPalette::from_image(&gray_img);
        assert!(gray_pal.accent.r() > 150 && gray_pal.accent_bright.r() > gray_pal.accent.r());
    }
}
