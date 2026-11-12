use egui::{ColorImage, Context, TextureHandle, TextureOptions, Vec2};
use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const KAINE_GIF_BYTES: &[u8] = include_bytes!("../assets/kaine-cry.gif");

pub struct GifPlayer {
    frames: Vec<(ColorImage, Duration)>,
    textures: Vec<TextureHandle>,
    current_index: usize,
    last_frame_time: Instant,
    dimensions: (usize, usize),
}

impl GifPlayer {
    /// Decodes raw GIF bytes and pre-caches all frame images into memory.
    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let decoder =
            GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("GIF decode error: {}", e))?;
        let raw_frames = decoder
            .into_frames()
            .collect_frames()
            .map_err(|e| format!("Collect frames error: {}", e))?;

        if raw_frames.is_empty() {
            return Err("GIF contains no frames".to_string());
        }

        let mut frames = Vec::with_capacity(raw_frames.len());
        let mut first_dims = (0, 0);

        for (i, frame) in raw_frames.into_iter().enumerate() {
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

        Ok(Self {
            frames,
            textures: Vec::new(),
            current_index: 0,
            last_frame_time: Instant::now(),
            dimensions: first_dims,
        })
    }

    /// Loads the GIF from data/assets, assets, or falls back to embedded bytes.
    pub fn load_default() -> Self {
        let mut candidates: Vec<PathBuf> = Vec::new();

        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            candidates.push(exe_dir.join("data").join("assets").join("kaine-cry.gif"));
            candidates.push(exe_dir.join("assets").join("kaine-cry.gif"));
        }
        candidates.push(PathBuf::from("data/assets/kaine-cry.gif"));
        candidates.push(PathBuf::from("assets/kaine-cry.gif"));

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

    /// Loads the embedded kaine-cry.gif compiled into the binary.
    pub fn new_embedded() -> Self {
        Self::load_from_bytes(KAINE_GIF_BYTES).expect("Failed to load embedded kaine-cry.gif")
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

    /// Pre-caches all decoded frame buffers into GPU texture handles.
    pub fn prepare_textures(&mut self, ctx: &Context) {
        if self.textures.is_empty() && !self.frames.is_empty() {
            self.textures = self
                .frames
                .iter()
                .enumerate()
                .map(|(i, (img, _))| {
                    ctx.load_texture(
                        format!("kaine_frame_{}", i),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_gif_decoding() {
        let player = GifPlayer::new_embedded();
        assert_eq!(player.frame_count(), 29);
        assert_eq!(player.dimensions(), (498, 379));
        assert_eq!(player.size(), Vec2::new(498.0, 379.0));
    }

    #[test]
    fn test_load_default() {
        let player = GifPlayer::load_default();
        assert_eq!(player.frame_count(), 29);
        assert_eq!(player.dimensions(), (498, 379));
    }
}
