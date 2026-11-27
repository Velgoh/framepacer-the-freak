use crate::gif_player::GifPlayer;
use egui::{Color32, FontId, Pos2, Rect, Rounding, Stroke, Vec2};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const DEFAULT_WINDOW_WIDTH: f32 = 300.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 228.0;

/// Window position data stored strictly in data/window.json.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct WindowPosition {
    pub x: f32,
    pub y: f32,
}

pub fn get_window_json_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let data_dir = parent.join("data");
        let _ = std::fs::create_dir_all(&data_dir);
        return data_dir.join("window.json");
    }
    let local_data = PathBuf::from("data");
    let _ = std::fs::create_dir_all(&local_data);
    local_data.join("window.json")
}

/// Clamps a saved window position to the current virtual desktop bounds for `win_size`.
pub fn clamp_to_virtual_screen(pos: [f32; 2], win_size: [f32; 2]) -> [f32; 2] {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN) as f32;
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN) as f32;
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN) as f32;
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN) as f32;
        let w = win_size[0].max(DEFAULT_WINDOW_WIDTH * 0.5);
        let h = win_size[1].max(DEFAULT_WINDOW_HEIGHT * 0.5);

        if vw >= w && vh >= h {
            let max_x = vx + vw - w;
            let max_y = vy + vh - h;
            [pos[0].clamp(vx, max_x), pos[1].clamp(vy, max_y)]
        } else {
            pos
        }
    }
}

pub fn load_window_position(win_size: [f32; 2]) -> Option<[f32; 2]> {
    let path = get_window_json_path();
    if let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(pos) = serde_json::from_str::<WindowPosition>(&content)
        && pos.x.is_finite()
        && pos.y.is_finite()
    {
        return Some(clamp_to_virtual_screen([pos.x, pos.y], win_size));
    }
    None
}

pub fn save_window_position(pos: [f32; 2]) {
    let path = get_window_json_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = WindowPosition {
        x: pos[0],
        y: pos[1],
    };
    if let Ok(json) = serde_json::to_string_pretty(&data) {
        let _ = std::fs::write(&path, json);
    }
}

/// Thread-safe telemetry shared between the pacing engine and the GUI.
#[derive(Debug, Clone)]
pub struct UiTelemetry {
    pub auto_enabled: bool,
    pub active_game_name: Option<String>,
    pub active_game_pid: Option<u32>,
    pub current_cap: u32,
    pub measured_fps: Option<f64>,
    pub is_measuring_native: bool,
    pub pacing_status: String,
    pub ipc_connected: bool,
    pub in_drop_state: bool,
    pub etw_active: bool,
    pub is_elevated: bool,
    pub etw_error: Option<String>,
    pub gpu_util_pct: Option<f64>,
    pub cpu_thread_pct: Option<f64>,
}

impl Default for UiTelemetry {
    fn default() -> Self {
        Self {
            auto_enabled: true,
            active_game_name: None,
            active_game_pid: None,
            current_cap: 0,
            measured_fps: None,
            is_measuring_native: false,
            pacing_status: "STANDBY".to_string(),
            ipc_connected: false,
            in_drop_state: false,
            etw_active: true,
            is_elevated: false,
            etw_error: None,
            gpu_util_pct: None,
            cpu_thread_pct: None,
        }
    }
}

pub enum UiCommand {
    ToggleAuto,
}

pub struct FreakApp {
    telemetry: Arc<Mutex<UiTelemetry>>,
    command_sender: std::sync::mpsc::Sender<UiCommand>,
    gif_player: GifPlayer,
    last_saved_pos: Option<[f32; 2]>,
    last_seen_pos: Option<[f32; 2]>,
    pos_last_moved: Option<Instant>,
    style_initialized: bool,
    gif_picker_open: bool,
    gif_input: String,
    gif_status: Option<(String, bool)>,
    gif_loading: bool,
    gif_rx: Option<std::sync::mpsc::Receiver<Result<GifPlayer, String>>>,
    browse_rx: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
    pending_unresizable: bool,
}

impl FreakApp {
    pub fn new(
        telemetry: Arc<Mutex<UiTelemetry>>,
        command_sender: std::sync::mpsc::Sender<UiCommand>,
    ) -> Self {
        Self::with_gif_player(telemetry, command_sender, GifPlayer::load_default())
    }

    pub fn with_gif_player(
        telemetry: Arc<Mutex<UiTelemetry>>,
        command_sender: std::sync::mpsc::Sender<UiCommand>,
        gif_player: GifPlayer,
    ) -> Self {
        let initial_size = gif_player.target_window_size();
        let saved = load_window_position(initial_size);
        Self {
            telemetry,
            command_sender,
            gif_player,
            last_saved_pos: saved,
            last_seen_pos: saved,
            pos_last_moved: None,
            style_initialized: false,
            gif_picker_open: false,
            gif_input: String::new(),
            gif_status: None,
            gif_loading: false,
            gif_rx: None,
            browse_rx: None,
            pending_unresizable: false,
        }
    }

    fn apply_gif_player(&mut self, ctx: &egui::Context, new_player: GifPlayer) {
        self.gif_player = new_player;
        let new_size = self.gif_player.target_window_size();
        ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(new_size.into()));
        self.pending_unresizable = true;
    }

    fn start_gif_load(&mut self, source: String) {
        if self.gif_loading {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.gif_rx = Some(rx);
        self.gif_loading = true;
        self.gif_status = Some(("Loading GIF...".to_string(), false));
        std::thread::spawn(move || {
            let res = crate::gif_player::load_custom_gif_source(&source);
            let _ = tx.send(res);
        });
    }

    fn start_browse_dialog(&mut self) {
        if self.browse_rx.is_some() || self.gif_loading {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.browse_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(crate::gif_player::pick_gif_file_dialog());
        });
    }
}

impl Drop for FreakApp {
    fn drop(&mut self) {
        if let Some(pos) = self.last_seen_pos {
            save_window_position(pos);
        }
    }
}

// Aesthetic palette derived from kaine-cry.gif & NieR Replicant
const COLOR_DIM_IRON: Color32 = Color32::from_rgb(38, 45, 54); // #262d36
const COLOR_DIM_IRON_CORE: Color32 = Color32::from_rgb(26, 31, 38);
const COLOR_BG_DARK: Color32 = Color32::from_rgb(10, 11, 14); // #0a0b0e
const COLOR_STATUS_WARN: Color32 = Color32::from_rgb(212, 162, 89); // muted amber
const COLOR_STATUS_OFF: Color32 = Color32::from_rgb(196, 122, 122); // muted coral

fn truncate_name(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars {
        name.to_string()
    } else {
        let prefix: String = name.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", prefix)
    }
}

impl eframe::App for FreakApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.pending_unresizable {
            ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(false));
            self.pending_unresizable = false;
        }

        // Poll background GIF download/decode & native file dialog channels
        if let Some(rx) = &self.browse_rx
            && let Ok(res) = rx.try_recv()
        {
            self.browse_rx = None;
            if let Some(path) = res {
                let s = path.display().to_string();
                self.gif_input = s.clone();
                self.start_gif_load(s);
            }
        }

        if let Some(rx) = &self.gif_rx
            && let Ok(res) = rx.try_recv()
        {
            self.gif_rx = None;
            self.gif_loading = false;
            match res {
                Ok(new_player) => {
                    self.apply_gif_player(ctx, new_player);
                    self.gif_status = None;
                    self.gif_picker_open = false;
                }
                Err(err) => {
                    self.gif_status = Some((truncate_name(&err, 36), true));
                }
            }
        }

        // Allow dropping a .gif file directly onto the window
        let dropped_path = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.as_ref().map(|p| p.display().to_string()))
        });
        if let Some(path) = dropped_path {
            self.gif_input = path.clone();
            self.gif_picker_open = true;
            self.start_gif_load(path);
        }

        if self.gif_picker_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.gif_picker_open = false;
        }

        let Ok(telem) = self.telemetry.lock().map(|g| g.clone()) else {
            return;
        };

        // 1. Position tracking and debounced save to data/window.json
        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            let current_pos = [outer.min.x, outer.min.y];
            if current_pos[0].is_finite() && current_pos[1].is_finite() {
                if self.last_seen_pos != Some(current_pos) {
                    self.last_seen_pos = Some(current_pos);
                    self.pos_last_moved = Some(Instant::now());
                } else if let Some(last_moved) = self.pos_last_moved
                    && last_moved.elapsed() >= Duration::from_millis(500)
                    && self.last_saved_pos != Some(current_pos)
                {
                    save_window_position(current_pos);
                    self.last_saved_pos = Some(current_pos);
                    self.pos_last_moved = None;
                }
            }
        }

        // 2. Slow fade-in on startup (over 1.0 second)
        let fade_in = ctx.animate_value_with_time(egui::Id::new("freak_fade_in"), 1.0, 1.0);

        // 3. Hover state detection: controls visible only when cursor is inside window
        let has_pointer = ctx.input(|i| i.pointer.hover_pos().is_some());
        let hover_alpha = ctx.animate_bool(egui::Id::new("freak_hover_alpha"), has_pointer);

        if !self.style_initialized {
            let mut style = (*ctx.style()).clone();
            style.visuals.dark_mode = true;
            style.visuals.panel_fill = COLOR_BG_DARK;
            style.visuals.window_fill = COLOR_BG_DARK;
            ctx.set_style(style);
            self.style_initialized = true;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(0.0))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();

                // A. Full-bleed GIF Canvas (cover-cropped to preserve aspect ratio with zero black bars)
                let cover_uv = self.gif_player.compute_cover_uv(full_rect.size());
                let gif_tex = self.gif_player.update_and_get_texture(ctx);
                if let Some(tex) = gif_tex {
                    let base_tint = if telem.auto_enabled {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(140, 150, 165)
                    };
                    let tint = Color32::from_rgba_unmultiplied(
                        base_tint.r(),
                        base_tint.g(),
                        base_tint.b(),
                        (255.0 * fade_in) as u8,
                    );
                    ui.painter().image(tex.id(), full_rect, cover_uv, tint);
                }

                // B. Soft Perimeter Vignette (NieR Replicant / Cieris Somnia style)
                for i in 0..10 {
                    let inset_rect = full_rect.shrink(i as f32);
                    let v_alpha = (((10 - i) as f32 * 2.8) * fade_in) as u8;
                    ui.painter().rect_stroke(
                        inset_rect,
                        Rounding::ZERO,
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(8, 10, 14, v_alpha)),
                    );
                }

                // C. Subtle Procedural Film Grain Overlay (~12 fps flicker, adapts to window size)
                let palette = self.gif_player.palette();
                let grain_w = full_rect.width().max(1.0);
                let grain_h = full_rect.height().max(1.0);
                let grain_time = (ctx.input(|i| i.time) * 12.0) as u32;
                for i in 0..48 {
                    let hx = ((i * 1103515245 + grain_time * 12345) & 0x7fffffff) as f32;
                    let hy = ((i * 214013 + grain_time * 2531011) & 0x7fffffff) as f32;
                    let gx = full_rect.min.x + (hx % grain_w);
                    let gy = full_rect.min.y + (hy % grain_h);
                    let g_alpha = ((5 + (i % 7)) as f32 * fade_in) as u8;
                    ui.painter().rect_filled(
                        Rect::from_min_size(Pos2::new(gx, gy), Vec2::new(1.0, 1.0)),
                        Rounding::ZERO,
                        Color32::from_rgba_unmultiplied(180, 205, 230, g_alpha),
                    );
                }

                // D. Four Thin Hairline Corner Brackets (┌ ┐ └ ┘)
                let corner_len = 6.0;
                let corner_inset = 2.5;
                let corner_col = Color32::from_rgba_unmultiplied(
                    palette.corner.r(),
                    palette.corner.g(),
                    palette.corner.b(),
                    (150.0 * fade_in) as u8,
                );
                let corner_stroke = Stroke::new(1.0_f32, corner_col);

                // Top-left ┌
                let tl = full_rect.min + Vec2::new(corner_inset, corner_inset);
                ui.painter()
                    .line_segment([tl, tl + Vec2::new(corner_len, 0.0)], corner_stroke);
                ui.painter()
                    .line_segment([tl, tl + Vec2::new(0.0, corner_len)], corner_stroke);

                // Top-right ┐
                let tr = Pos2::new(
                    full_rect.max.x - corner_inset,
                    full_rect.min.y + corner_inset,
                );
                ui.painter()
                    .line_segment([tr, tr + Vec2::new(-corner_len, 0.0)], corner_stroke);
                ui.painter()
                    .line_segment([tr, tr + Vec2::new(0.0, corner_len)], corner_stroke);

                // Bottom-left └
                let bl = Pos2::new(
                    full_rect.min.x + corner_inset,
                    full_rect.max.y - corner_inset,
                );
                ui.painter()
                    .line_segment([bl, bl + Vec2::new(corner_len, 0.0)], corner_stroke);
                ui.painter()
                    .line_segment([bl, bl + Vec2::new(0.0, -corner_len)], corner_stroke);

                // Bottom-right ┘
                let br = full_rect.max - Vec2::new(corner_inset, corner_inset);
                ui.painter()
                    .line_segment([br, br + Vec2::new(-corner_len, 0.0)], corner_stroke);
                ui.painter()
                    .line_segment([br, br + Vec2::new(0.0, -corner_len)], corner_stroke);

                // E. Sharp 90° Outer 1px Border
                ui.painter().rect_stroke(
                    full_rect,
                    Rounding::ZERO,
                    Stroke::new(1.0_f32, palette.border),
                );

                // F. Interactive Control Rectangles (Active on Hover or when GIF picker is open)
                let sigil_center = Pos2::new(full_rect.min.x + 20.0, full_rect.min.y + 20.0);
                let sigil_rect = Rect::from_center_size(sigil_center, Vec2::new(26.0, 26.0));

                let gif_btn_center = Pos2::new(full_rect.max.x - 46.0, full_rect.min.y + 16.0);
                let gif_btn_rect = Rect::from_center_size(gif_btn_center, Vec2::new(16.0, 16.0));

                let min_center = Pos2::new(full_rect.max.x - 30.0, full_rect.min.y + 16.0);
                let min_rect = Rect::from_center_size(min_center, Vec2::new(16.0, 16.0));

                let close_center = Pos2::new(full_rect.max.x - 14.0, full_rect.min.y + 16.0);
                let close_rect = Rect::from_center_size(close_center, Vec2::new(16.0, 16.0));

                let show_controls = (has_pointer && hover_alpha > 0.1) || self.gif_picker_open;
                let (sigil_resp, gif_btn_resp, min_resp, close_resp) = if show_controls {
                    (
                        ui.allocate_rect(sigil_rect, egui::Sense::click()),
                        ui.allocate_rect(gif_btn_rect, egui::Sense::click()),
                        ui.allocate_rect(min_rect, egui::Sense::click()),
                        ui.allocate_rect(close_rect, egui::Sense::click()),
                    )
                } else {
                    (
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                    )
                };

                // Window dragging anywhere on canvas when picker is closed and cursor is off controls
                if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                    let on_interactive = hover_alpha > 0.1
                        && (sigil_rect.contains(pos)
                            || gif_btn_rect.contains(pos)
                            || min_rect.contains(pos)
                            || close_rect.contains(pos));
                    if !self.gif_picker_open
                        && !on_interactive
                        && ctx.input(|i| i.pointer.primary_pressed())
                    {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                }

                // Handle cursor style
                if sigil_resp.hovered()
                    || gif_btn_resp.hovered()
                    || min_resp.hovered()
                    || close_resp.hovered()
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                // Handle control clicks
                if sigil_resp.clicked() {
                    let _ = self.command_sender.send(UiCommand::ToggleAuto);
                    if let Ok(mut t) = self.telemetry.lock() {
                        t.auto_enabled = !t.auto_enabled;
                    }
                }
                if gif_btn_resp.clicked() {
                    self.gif_picker_open = !self.gif_picker_open;
                    if self.gif_picker_open && !self.gif_loading {
                        self.gif_status = None;
                    }
                }
                if min_resp.clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                if close_resp.clicked() {
                    if let Some(pos) = self.last_seen_pos {
                        save_window_position(pos);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                // G. Render Hover-Only Controls when hover_alpha > 0.01 or picker open
                let ctrl_alpha = if self.gif_picker_open {
                    hover_alpha.max(0.85)
                } else {
                    hover_alpha
                };
                if ctrl_alpha > 0.01 {
                    // 1. Auto Toggle Sigil (Geometric NieR Rune)
                    let r_outer = 8.0;
                    let is_sigil_hovered = sigil_resp.hovered();

                    // Optional pulse flicker if measuring native
                    let sigil_pulse = if telem.is_measuring_native {
                        0.5 + 0.5 * (ctx.input(|i| i.time) * 16.0).sin().abs() as f32
                    } else {
                        1.0
                    };

                    if telem.auto_enabled {
                        let halo_alpha_1 = ((if is_sigil_hovered { 45.0 } else { 24.0 })
                            * hover_alpha
                            * sigil_pulse) as u8;
                        let halo_alpha_2 = ((if is_sigil_hovered { 75.0 } else { 48.0 })
                            * hover_alpha
                            * sigil_pulse) as u8;

                        ui.painter().circle_filled(
                            sigil_center,
                            13.0,
                            Color32::from_rgba_unmultiplied(
                                palette.accent.r(),
                                palette.accent.g(),
                                palette.accent.b(),
                                halo_alpha_1,
                            ),
                        );
                        ui.painter().circle_filled(
                            sigil_center,
                            10.0,
                            Color32::from_rgba_unmultiplied(
                                palette.accent.r(),
                                palette.accent.g(),
                                palette.accent.b(),
                                halo_alpha_2,
                            ),
                        );

                        // Outer diamond contour
                        let p_top = sigil_center + Vec2::new(0.0, -r_outer);
                        let p_right = sigil_center + Vec2::new(r_outer, 0.0);
                        let p_bottom = sigil_center + Vec2::new(0.0, r_outer);
                        let p_left = sigil_center + Vec2::new(-r_outer, 0.0);

                        let stroke_base = if is_sigil_hovered {
                            palette.accent_bright
                        } else {
                            palette.accent
                        };
                        let stroke_col = Color32::from_rgba_unmultiplied(
                            stroke_base.r(),
                            stroke_base.g(),
                            stroke_base.b(),
                            (255.0 * hover_alpha * sigil_pulse) as u8,
                        );
                        let stroke = Stroke::new(1.2_f32, stroke_col);
                        ui.painter().line_segment([p_top, p_right], stroke);
                        ui.painter().line_segment([p_right, p_bottom], stroke);
                        ui.painter().line_segment([p_bottom, p_left], stroke);
                        ui.painter().line_segment([p_left, p_top], stroke);

                        // Inner geometric ring
                        ui.painter().circle_stroke(
                            sigil_center,
                            4.0,
                            Stroke::new(
                                1.0_f32,
                                Color32::from_rgba_unmultiplied(
                                    palette.accent_bright.r(),
                                    palette.accent_bright.g(),
                                    palette.accent_bright.b(),
                                    (180.0 * hover_alpha) as u8,
                                ),
                            ),
                        );

                        // Central core
                        let core_col = if is_sigil_hovered {
                            Color32::from_rgba_unmultiplied(
                                255,
                                255,
                                255,
                                (255.0 * hover_alpha) as u8,
                            )
                        } else {
                            Color32::from_rgba_unmultiplied(
                                225,
                                245,
                                255,
                                (220.0 * hover_alpha) as u8,
                            )
                        };
                        ui.painter().circle_filled(sigil_center, 1.6, core_col);
                    } else {
                        // Dormant iron sigil
                        let p_top = sigil_center + Vec2::new(0.0, -r_outer);
                        let p_right = sigil_center + Vec2::new(r_outer, 0.0);
                        let p_bottom = sigil_center + Vec2::new(0.0, r_outer);
                        let p_left = sigil_center + Vec2::new(-r_outer, 0.0);

                        let stroke_col = Color32::from_rgba_unmultiplied(
                            COLOR_DIM_IRON.r(),
                            COLOR_DIM_IRON.g(),
                            COLOR_DIM_IRON.b(),
                            (220.0 * hover_alpha) as u8,
                        );
                        let stroke = Stroke::new(1.0_f32, stroke_col);
                        ui.painter().line_segment([p_top, p_right], stroke);
                        ui.painter().line_segment([p_right, p_bottom], stroke);
                        ui.painter().line_segment([p_bottom, p_left], stroke);
                        ui.painter().line_segment([p_left, p_top], stroke);

                        ui.painter().circle_stroke(
                            sigil_center,
                            4.0,
                            Stroke::new(
                                0.8_f32,
                                Color32::from_rgba_unmultiplied(
                                    30,
                                    36,
                                    44,
                                    (200.0 * hover_alpha) as u8,
                                ),
                            ),
                        );
                        ui.painter().circle_filled(
                            sigil_center,
                            1.4,
                            Color32::from_rgba_unmultiplied(
                                COLOR_DIM_IRON_CORE.r(),
                                COLOR_DIM_IRON_CORE.g(),
                                COLOR_DIM_IRON_CORE.b(),
                                (200.0 * hover_alpha) as u8,
                            ),
                        );
                    }

                    // 2. GIF Switcher Glyph (Hairline picture frame)
                    let gif_alpha = if gif_btn_resp.hovered() || self.gif_picker_open {
                        245.0
                    } else {
                        130.0
                    };
                    let gif_col = if self.gif_picker_open {
                        Color32::from_rgba_unmultiplied(
                            palette.accent.r(),
                            palette.accent.g(),
                            palette.accent.b(),
                            (gif_alpha * ctrl_alpha) as u8,
                        )
                    } else {
                        Color32::from_rgba_unmultiplied(
                            180,
                            200,
                            220,
                            (gif_alpha * ctrl_alpha) as u8,
                        )
                    };
                    let frame_rect = Rect::from_center_size(gif_btn_center, Vec2::new(9.0, 7.0));
                    ui.painter().rect_stroke(
                        frame_rect,
                        Rounding::ZERO,
                        Stroke::new(1.0_f32, gif_col),
                    );
                    ui.painter().line_segment(
                        [
                            frame_rect.left_bottom() + Vec2::new(1.5, -1.5),
                            frame_rect.center() + Vec2::new(-0.5, 0.0),
                        ],
                        Stroke::new(1.0_f32, gif_col),
                    );
                    ui.painter().line_segment(
                        [
                            frame_rect.center() + Vec2::new(-0.5, 0.0),
                            frame_rect.right_bottom() + Vec2::new(-1.5, -2.5),
                        ],
                        Stroke::new(1.0_f32, gif_col),
                    );

                    // 3. Minimize Glyph (Hairline bar)
                    let min_alpha = if min_resp.hovered() { 240.0 } else { 130.0 };
                    let min_col = Color32::from_rgba_unmultiplied(
                        180,
                        200,
                        220,
                        (min_alpha * ctrl_alpha) as u8,
                    );
                    ui.painter().line_segment(
                        [
                            min_center + Vec2::new(-3.5, 0.0),
                            min_center + Vec2::new(3.5, 0.0),
                        ],
                        Stroke::new(1.0_f32, min_col),
                    );

                    // 4. Close Glyph (Hairline cross)
                    let close_alpha = if close_resp.hovered() { 240.0 } else { 130.0 };
                    let close_col = Color32::from_rgba_unmultiplied(
                        220,
                        190,
                        190,
                        (close_alpha * ctrl_alpha) as u8,
                    );
                    let cs = 3.0;
                    ui.painter().line_segment(
                        [
                            close_center + Vec2::new(-cs, -cs),
                            close_center + Vec2::new(cs, cs),
                        ],
                        Stroke::new(1.0_f32, close_col),
                    );
                    ui.painter().line_segment(
                        [
                            close_center + Vec2::new(-cs, cs),
                            close_center + Vec2::new(cs, -cs),
                        ],
                        Stroke::new(1.0_f32, close_col),
                    );
                }

                // Translucent Custom GIF Picker Overlay (adapts width/height to any GIF window)
                if self.gif_picker_open {
                    let card_w = (full_rect.width() - 18.0).clamp(182.0, 268.0);
                    let card_h = 118.0_f32.min((full_rect.height() - 40.0).max(106.0));
                    let card_rect =
                        Rect::from_center_size(full_rect.center(), Vec2::new(card_w, card_h));
                    ui.painter().rect_filled(
                        card_rect,
                        Rounding::same(2.0),
                        Color32::from_rgba_unmultiplied(10, 12, 16, 238),
                    );
                    ui.painter().rect_stroke(
                        card_rect,
                        Rounding::same(2.0),
                        Stroke::new(1.0_f32, palette.border),
                    );

                    ui.allocate_new_ui(
                        egui::UiBuilder::new().max_rect(card_rect.shrink(9.0)),
                        |ui| {
                            ui.spacing_mut().button_padding = Vec2::new(5.0, 2.0);
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("CUSTOM GIF")
                                        .size(10.0)
                                        .color(palette.accent_bright),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new("ESC")
                                                .size(9.0)
                                                .color(COLOR_DIM_IRON),
                                        );
                                    },
                                );
                            });
                            ui.add_space(4.0);

                            let edit_resp = ui.add_enabled(
                                !self.gif_loading,
                                egui::TextEdit::singleline(&mut self.gif_input)
                                    .hint_text("GIF URL or file path...")
                                    .font(FontId::proportional(10.5))
                                    .desired_width(f32::INFINITY),
                            );
                            if edit_resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && !self.gif_input.trim().is_empty()
                            {
                                self.start_gif_load(self.gif_input.clone());
                            }
                            ui.add_space(6.0);

                            ui.horizontal(|ui| {
                                let can_load =
                                    !self.gif_loading && !self.gif_input.trim().is_empty();
                                if ui
                                    .add_enabled(
                                        can_load,
                                        egui::Button::new(
                                            egui::RichText::new("Load")
                                                .size(10.0)
                                                .color(palette.accent_bright),
                                        ),
                                    )
                                    .clicked()
                                {
                                    self.start_gif_load(self.gif_input.clone());
                                }
                                if ui
                                    .add_enabled(
                                        !self.gif_loading,
                                        egui::Button::new(
                                            egui::RichText::new("Browse")
                                                .size(10.0)
                                                .color(palette.silver),
                                        ),
                                    )
                                    .clicked()
                                {
                                    self.start_browse_dialog();
                                }
                                if ui
                                    .add_enabled(
                                        !self.gif_loading,
                                        egui::Button::new(
                                            egui::RichText::new("Reset")
                                                .size(10.0)
                                                .color(palette.silver),
                                        ),
                                    )
                                    .clicked()
                                {
                                    let default_player = GifPlayer::reset_custom_gif();
                                    self.apply_gif_player(ctx, default_player);
                                    self.gif_input.clear();
                                    self.gif_status = None;
                                    self.gif_picker_open = false;
                                }
                            });
                            ui.add_space(4.0);

                            let (msg, col) = match &self.gif_status {
                                Some((m, true)) => (m.as_str(), COLOR_STATUS_OFF),
                                Some((m, false)) => (m.as_str(), palette.accent),
                                None => ("URL, Tenor/Giphy, or drop GIF", palette.silver),
                            };
                            ui.label(egui::RichText::new(msg).size(9.0).color(col));
                        },
                    );
                }

                // H. Hover-only bottom diagnostics (IPC/ETW status, game, live FPS vs cap)
                if hover_alpha > 0.01 {
                    let scrim_h = 36.0;
                    for band in 0..6 {
                        let t = (band as f32 + 1.0) / 6.0;
                        let band_h = scrim_h / 6.0;
                        let y = full_rect.max.y - scrim_h + (band as f32) * band_h;
                        let a = (165.0 * t * hover_alpha * fade_in) as u8;
                        ui.painter().rect_filled(
                            Rect::from_min_size(
                                Pos2::new(full_rect.min.x + 1.0, y),
                                Vec2::new(full_rect.width() - 2.0, band_h + 0.5),
                            ),
                            Rounding::ZERO,
                            Color32::from_rgba_unmultiplied(8, 10, 14, a),
                        );
                    }

                    let alpha_u8 = (230.0 * hover_alpha * fade_in) as u8;
                    let sub_alpha_u8 = (195.0 * hover_alpha * fade_in) as u8;
                    let ipc_col = if telem.ipc_connected {
                        palette.accent
                    } else {
                        COLOR_STATUS_OFF
                    };
                    let etw_col = if telem.etw_active {
                        palette.accent
                    } else {
                        COLOR_STATUS_WARN
                    };

                    let line1_y = full_rect.max.y - 28.0;
                    let mut x = full_rect.min.x + 10.0;

                    // IPC dot + label
                    ui.painter().circle_filled(
                        Pos2::new(x + 2.5, line1_y + 5.5),
                        2.2,
                        Color32::from_rgba_unmultiplied(
                            ipc_col.r(),
                            ipc_col.g(),
                            ipc_col.b(),
                            alpha_u8,
                        ),
                    );
                    x += 7.5;
                    let ipc_galley = ui.painter().layout_no_wrap(
                        "IPC".to_string(),
                        FontId::new(9.5, egui::FontFamily::Monospace),
                        Color32::from_rgba_unmultiplied(
                            palette.silver.r(),
                            palette.silver.g(),
                            palette.silver.b(),
                            alpha_u8,
                        ),
                    );
                    let ipc_w = ipc_galley.size().x;
                    ui.painter()
                        .galley(Pos2::new(x, line1_y), ipc_galley, Color32::WHITE);
                    x += ipc_w + 8.0;

                    // ETW dot + label
                    ui.painter().circle_filled(
                        Pos2::new(x + 2.5, line1_y + 5.5),
                        2.2,
                        Color32::from_rgba_unmultiplied(
                            etw_col.r(),
                            etw_col.g(),
                            etw_col.b(),
                            alpha_u8,
                        ),
                    );
                    x += 7.5;
                    let etw_galley = ui.painter().layout_no_wrap(
                        "ETW".to_string(),
                        FontId::new(9.5, egui::FontFamily::Monospace),
                        Color32::from_rgba_unmultiplied(
                            palette.silver.r(),
                            palette.silver.g(),
                            palette.silver.b(),
                            alpha_u8,
                        ),
                    );
                    let etw_w = etw_galley.size().x;
                    ui.painter()
                        .galley(Pos2::new(x, line1_y), etw_galley, Color32::WHITE);
                    x += etw_w + 6.0;

                    let max_game_chars =
                        ((full_rect.width() - 125.0) / 5.5).clamp(8.0, 24.0) as usize;
                    let target_label = if let Some(ref g) = telem.active_game_name {
                        truncate_name(g, max_game_chars)
                    } else if !telem.ipc_connected {
                        "NO FRAMEPACER".to_string()
                    } else {
                        "WAITING".to_string()
                    };
                    let target_galley = ui.painter().layout_no_wrap(
                        format!("· {}", target_label),
                        FontId::new(9.5, egui::FontFamily::Proportional),
                        Color32::from_rgba_unmultiplied(
                            palette.accent_bright.r(),
                            palette.accent_bright.g(),
                            palette.accent_bright.b(),
                            sub_alpha_u8,
                        ),
                    );
                    ui.painter()
                        .galley(Pos2::new(x, line1_y), target_galley, Color32::WHITE);

                    // Line 2: Pacing status + live FPS + GPU% (or admin warning if ETW failed un-elevated)
                    let line2_text = if !telem.is_elevated && !telem.etw_active {
                        "NEED ADMIN PRIVILEGES".to_string()
                    } else if telem.active_game_name.is_some() {
                        let fps_part = telem
                            .measured_fps
                            .map(|fps| format!(" · {:.0} FPS", fps))
                            .unwrap_or_default();
                        let gpu_part = telem
                            .gpu_util_pct
                            .map(|g| format!(" · G{:.0}%", g))
                            .unwrap_or_default();
                        format!("{}{}{}", telem.pacing_status, fps_part, gpu_part)
                    } else {
                        telem.pacing_status.clone()
                    };
                    let line2_col = if !telem.is_elevated && !telem.etw_active {
                        Color32::from_rgba_unmultiplied(
                            COLOR_STATUS_WARN.r(),
                            COLOR_STATUS_WARN.g(),
                            COLOR_STATUS_WARN.b(),
                            alpha_u8,
                        )
                    } else {
                        Color32::from_rgba_unmultiplied(
                            palette.silver.r(),
                            palette.silver.g(),
                            palette.silver.b(),
                            sub_alpha_u8,
                        )
                    };
                    let line2_galley = ui.painter().layout_no_wrap(
                        line2_text,
                        FontId::new(9.5, egui::FontFamily::Proportional),
                        line2_col,
                    );
                    ui.painter().galley(
                        Pos2::new(full_rect.min.x + 10.0, full_rect.max.y - 16.0),
                        line2_galley,
                        line2_col,
                    );
                }

                // I. Bottom-Right: Faint Adaptive FPS Numeral with subtle 1px shadow for bright GIF legibility
                let numeral_str = if telem.current_cap > 0 {
                    format!("{}", telem.current_cap)
                } else {
                    "—".to_string()
                };

                // Brief glyph flicker or number fade while the limiter-off benchmark runs
                let pulse_flicker = if telem.is_measuring_native {
                    0.35 + 0.35 * (ctx.input(|i| i.time) * 18.0).sin().abs() as f32
                } else {
                    1.0
                };

                let base_num_alpha = if telem.auto_enabled { 155.0 } else { 80.0 };
                let num_alpha = ((base_num_alpha * pulse_flicker * fade_in) as u8).max(12);
                let num_color = Color32::from_rgba_unmultiplied(
                    palette.silver.r(),
                    palette.silver.g(),
                    palette.silver.b(),
                    num_alpha,
                );
                let shadow_color = Color32::from_rgba_unmultiplied(
                    0,
                    0,
                    0,
                    ((num_alpha as f32) * 0.75) as u8,
                );

                let font_id = FontId::new(13.0, egui::FontFamily::Proportional);
                let shadow_galley =
                    ui.painter()
                        .layout_no_wrap(numeral_str.clone(), font_id.clone(), shadow_color);
                let galley = ui.painter().layout_no_wrap(numeral_str, font_id, num_color);

                let margin_right = 10.0;
                let margin_bottom = 8.0;
                let text_pos = Pos2::new(
                    full_rect.max.x - margin_right - galley.size().x,
                    full_rect.max.y - margin_bottom - galley.size().y,
                );

                if !telem.etw_active {
                    let dot_alpha = ((160.0 * fade_in) as u8).max(20);
                    ui.painter().circle_filled(
                        Pos2::new(text_pos.x - 6.0, text_pos.y + galley.size().y * 0.55),
                        2.2,
                        Color32::from_rgba_unmultiplied(
                            COLOR_STATUS_WARN.r(),
                            COLOR_STATUS_WARN.g(),
                            COLOR_STATUS_WARN.b(),
                            dot_alpha,
                        ),
                    );
                }

                // Render 1px shadow + adaptive numeral directly on canvas (no backing box or pill)
                ui.painter().galley(
                    text_pos + Vec2::new(1.0, 1.0),
                    shadow_galley,
                    shadow_color,
                );
                ui.painter().galley(text_pos, galley, num_color);
            });

        // Bounded 100ms repaint fallback for telemetry / hover fade updates (GIF timing is driven by gif_player)
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

pub fn run_gui(
    telemetry: Arc<Mutex<UiTelemetry>>,
    command_sender: std::sync::mpsc::Sender<UiCommand>,
) -> Result<(), eframe::Error> {
    let initial_player = GifPlayer::load_default();
    let initial_size = initial_player.target_window_size();

    let mut builder = egui::ViewportBuilder::default()
        .with_inner_size(initial_size)
        .with_min_inner_size([180.0, 150.0])
        .with_max_inner_size([420.0, 400.0])
        .with_resizable(false)
        .with_decorations(false)
        .with_always_on_top()
        .with_title("FramePacer Companion");

    if let Some([x, y]) = load_window_position(initial_size) {
        builder = builder.with_position([x, y]);
    }

    let native_options = eframe::NativeOptions {
        viewport: builder,
        ..Default::default()
    };

    eframe::run_native(
        "framepacer-companion",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(FreakApp::with_gif_player(
                telemetry,
                command_sender,
                initial_player,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_position_serde() {
        let pos = WindowPosition { x: 240.5, y: 180.0 };
        let json = serde_json::to_string(&pos).expect("serialize");
        let decoded: WindowPosition = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.x, 240.5);
        assert_eq!(decoded.y, 180.0);
    }

    #[test]
    fn test_fps_numeral_formatting() {
        let telem_49 = UiTelemetry {
            current_cap: 49,
            ..Default::default()
        };
        let s_49 = if telem_49.current_cap > 0 {
            format!("{}", telem_49.current_cap)
        } else {
            "—".to_string()
        };
        assert_eq!(s_49, "49");

        let telem_zero = UiTelemetry {
            current_cap: 0,
            ..Default::default()
        };
        let s_zero = if telem_zero.current_cap > 0 {
            format!("{}", telem_zero.current_cap)
        } else {
            "—".to_string()
        };
        assert_eq!(s_zero, "—");
    }

    #[test]
    fn test_truncate_name_and_clamp_bounds() {
        assert_eq!(truncate_name("Short.exe", 20), "Short.exe");
        assert_eq!(
            truncate_name("MonsterHunterWorld.exe", 12),
            "MonsterHunt…"
        );
        let clamped = clamp_to_virtual_screen([0.0, 0.0], [300.0, 228.0]);
        assert!(clamped[0].is_finite() && clamped[1].is_finite());
    }
}
