use crate::gif_player::GifPlayer;
use egui::{Color32, FontId, Pos2, Rect, Rounding, Stroke, Vec2};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

pub fn load_window_position() -> Option<[f32; 2]> {
    let path = get_window_json_path();
    if let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(pos) = serde_json::from_str::<WindowPosition>(&content)
    {
        return Some([pos.x, pos.y]);
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
}

impl Default for UiTelemetry {
    fn default() -> Self {
        Self {
            auto_enabled: true,
            active_game_name: None,
            active_game_pid: None,
            current_cap: 60,
            measured_fps: None,
            is_measuring_native: false,
            pacing_status: "STANDBY".to_string(),
            ipc_connected: false,
            in_drop_state: false,
            etw_active: true,
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
}

impl FreakApp {
    pub fn new(
        telemetry: Arc<Mutex<UiTelemetry>>,
        command_sender: std::sync::mpsc::Sender<UiCommand>,
    ) -> Self {
        let saved = load_window_position();
        Self {
            telemetry,
            command_sender,
            gif_player: GifPlayer::load_default(),
            last_saved_pos: saved,
            last_seen_pos: saved,
            pos_last_moved: None,
        }
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
const COLOR_LUNAR_CYAN: Color32 = Color32::from_rgb(126, 174, 196); // #7eaec4
const COLOR_LUNAR_GLOW_BRIGHT: Color32 = Color32::from_rgb(164, 200, 217); // #a4c8d9
const COLOR_LUNAR_SILVER: Color32 = Color32::from_rgb(143, 162, 181); // #8fa2b5
const COLOR_DIM_IRON: Color32 = Color32::from_rgb(38, 45, 54); // #262d36
const COLOR_DIM_IRON_CORE: Color32 = Color32::from_rgb(26, 31, 38);
const COLOR_BORDER: Color32 = Color32::from_rgb(54, 65, 78); // #36414e
const COLOR_BG_DARK: Color32 = Color32::from_rgb(10, 11, 14); // #0a0b0e

impl eframe::App for FreakApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let telem = self.telemetry.lock().unwrap().clone();

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

        let mut style = (*ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = COLOR_BG_DARK;
        style.visuals.window_fill = COLOR_BG_DARK;
        ctx.set_style(style);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(0.0))
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();

                // A. Full-bleed GIF Canvas (zero black bars, exact aspect ratio)
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
                    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                    ui.painter().image(tex.id(), full_rect, uv, tint);
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

                // C. Subtle Procedural Film Grain Overlay (~12 fps flicker)
                let grain_time = (ctx.input(|i| i.time) * 12.0) as u32;
                for i in 0..48 {
                    let hx = ((i * 1103515245 + grain_time * 12345) & 0x7fffffff) as f32;
                    let hy = ((i * 214013 + grain_time * 2531011) & 0x7fffffff) as f32;
                    let gx = full_rect.min.x + (hx % 300.0);
                    let gy = full_rect.min.y + (hy % 228.0);
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
                let corner_col =
                    Color32::from_rgba_unmultiplied(74, 91, 109, (150.0 * fade_in) as u8);
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
                    Stroke::new(1.0_f32, COLOR_BORDER),
                );

                // F. Interactive Control Rectangles (Active only on Hover)
                let sigil_center = Pos2::new(full_rect.min.x + 20.0, full_rect.min.y + 20.0);
                let sigil_rect = Rect::from_center_size(sigil_center, Vec2::new(26.0, 26.0));

                let min_center = Pos2::new(full_rect.max.x - 30.0, full_rect.min.y + 16.0);
                let min_rect = Rect::from_center_size(min_center, Vec2::new(16.0, 16.0));

                let close_center = Pos2::new(full_rect.max.x - 14.0, full_rect.min.y + 16.0);
                let close_rect = Rect::from_center_size(close_center, Vec2::new(16.0, 16.0));

                let (sigil_resp, min_resp, close_resp) = if has_pointer && hover_alpha > 0.1 {
                    (
                        ui.allocate_rect(sigil_rect, egui::Sense::click()),
                        ui.allocate_rect(min_rect, egui::Sense::click()),
                        ui.allocate_rect(close_rect, egui::Sense::click()),
                    )
                } else {
                    (
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                        ui.allocate_rect(Rect::NOTHING, egui::Sense::hover()),
                    )
                };

                // Window dragging anywhere on unhovered canvas or non-interactive areas
                if let Some(pos) = ctx.input(|i| i.pointer.latest_pos()) {
                    let on_interactive = hover_alpha > 0.1
                        && (sigil_rect.contains(pos)
                            || min_rect.contains(pos)
                            || close_rect.contains(pos));
                    if !on_interactive && ctx.input(|i| i.pointer.primary_pressed()) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                }

                // Handle cursor style
                if sigil_resp.hovered() || min_resp.hovered() || close_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                // Handle control clicks
                if sigil_resp.clicked() {
                    let _ = self.command_sender.send(UiCommand::ToggleAuto);
                    if let Ok(mut t) = self.telemetry.lock() {
                        t.auto_enabled = !t.auto_enabled;
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

                // G. Render Hover-Only Controls when hover_alpha > 0.0
                if hover_alpha > 0.01 {
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
                            Color32::from_rgba_unmultiplied(126, 174, 196, halo_alpha_1),
                        );
                        ui.painter().circle_filled(
                            sigil_center,
                            10.0,
                            Color32::from_rgba_unmultiplied(126, 174, 196, halo_alpha_2),
                        );

                        // Outer diamond contour
                        let p_top = sigil_center + Vec2::new(0.0, -r_outer);
                        let p_right = sigil_center + Vec2::new(r_outer, 0.0);
                        let p_bottom = sigil_center + Vec2::new(0.0, r_outer);
                        let p_left = sigil_center + Vec2::new(-r_outer, 0.0);

                        let stroke_base = if is_sigil_hovered {
                            COLOR_LUNAR_GLOW_BRIGHT
                        } else {
                            COLOR_LUNAR_CYAN
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
                                    164,
                                    200,
                                    217,
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

                    // 2. Minimize Glyph (Hairline bar)
                    let min_alpha = if min_resp.hovered() { 240.0 } else { 130.0 };
                    let min_col = Color32::from_rgba_unmultiplied(
                        180,
                        200,
                        220,
                        (min_alpha * hover_alpha) as u8,
                    );
                    ui.painter().line_segment(
                        [
                            min_center + Vec2::new(-3.5, 0.0),
                            min_center + Vec2::new(3.5, 0.0),
                        ],
                        Stroke::new(1.0_f32, min_col),
                    );

                    // 3. Close Glyph (Hairline cross)
                    let close_alpha = if close_resp.hovered() { 240.0 } else { 130.0 };
                    let close_col = Color32::from_rgba_unmultiplied(
                        220,
                        190,
                        190,
                        (close_alpha * hover_alpha) as u8,
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

                // H. Bottom-Right: Thin, Faint Moonbeam FPS Numeral (Zero words, NO backing box)
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

                let base_num_alpha = if telem.auto_enabled { 130.0 } else { 65.0 };
                let num_alpha = ((base_num_alpha * pulse_flicker * fade_in) as u8).max(10);
                let num_color = Color32::from_rgba_unmultiplied(
                    COLOR_LUNAR_SILVER.r(),
                    COLOR_LUNAR_SILVER.g(),
                    COLOR_LUNAR_SILVER.b(),
                    num_alpha,
                );

                let font_id = FontId::new(13.0, egui::FontFamily::Proportional);
                let galley = ui.painter().layout_no_wrap(numeral_str, font_id, num_color);

                let margin_right = 10.0;
                let margin_bottom = 8.0;
                let text_pos = Pos2::new(
                    full_rect.max.x - margin_right - galley.size().x,
                    full_rect.max.y - margin_bottom - galley.size().y,
                );

                // Render faint numeral directly on canvas (no backing box or pill)
                ui.painter().galley(text_pos, galley, num_color);
            });

        // Request continuous repaint for smooth GIF playback and animation
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

pub fn run_gui(
    telemetry: Arc<Mutex<UiTelemetry>>,
    command_sender: std::sync::mpsc::Sender<UiCommand>,
) -> Result<(), eframe::Error> {
    let mut builder = egui::ViewportBuilder::default()
        .with_inner_size([300.0, 228.0])
        .with_min_inner_size([300.0, 228.0])
        .with_max_inner_size([300.0, 228.0])
        .with_resizable(false)
        .with_decorations(false)
        .with_always_on_top()
        .with_title("THE FREAK");

    if let Some([x, y]) = load_window_position() {
        builder = builder.with_position([x, y]);
    }

    let native_options = eframe::NativeOptions {
        viewport: builder,
        ..Default::default()
    };

    eframe::run_native(
        "framepacer-the-freak",
        native_options,
        Box::new(|_cc| Ok(Box::new(FreakApp::new(telemetry, command_sender)))),
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
}
