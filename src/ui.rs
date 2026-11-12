use crate::gif_player::GifPlayer;
use egui::{Align, Color32, FontId, Layout, RichText, Rounding, Stroke, Vec2};
use std::sync::{Arc, Mutex};

/// Thread-safe telemetry shared between the pacing engine and the GUI.
#[derive(Debug, Clone)]
pub struct UiTelemetry {
    pub auto_enabled: bool,
    pub active_game_name: Option<String>,
    pub active_game_pid: Option<u32>,
    pub current_cap: u32,
    pub measured_fps: Option<f64>,
    pub pacing_status: String,
    pub ipc_connected: bool,
    pub in_drop_state: bool,
}

impl Default for UiTelemetry {
    fn default() -> Self {
        Self {
            auto_enabled: true,
            active_game_name: None,
            active_game_pid: None,
            current_cap: 60,
            measured_fps: None,
            pacing_status: "STANDBY".to_string(),
            ipc_connected: false,
            in_drop_state: false,
        }
    }
}

pub enum UiCommand {
    ToggleAuto,
    NudgeCap(i32),
}

pub struct CyberApp {
    telemetry: Arc<Mutex<UiTelemetry>>,
    command_sender: std::sync::mpsc::Sender<UiCommand>,
    gif_player: GifPlayer,
}

impl CyberApp {
    pub fn new(
        telemetry: Arc<Mutex<UiTelemetry>>,
        command_sender: std::sync::mpsc::Sender<UiCommand>,
    ) -> Self {
        Self {
            telemetry,
            command_sender,
            gif_player: GifPlayer::new_embedded(),
        }
    }
}

// Cyber aesthetic color palette
const COLOR_BG: Color32 = Color32::from_rgb(10, 12, 16);
const COLOR_CARD: Color32 = Color32::from_rgb(18, 22, 30);
const COLOR_CYAN: Color32 = Color32::from_rgb(0, 240, 255);
const COLOR_EMERALD: Color32 = Color32::from_rgb(0, 255, 170);
const COLOR_AMBER: Color32 = Color32::from_rgb(255, 184, 0);
const COLOR_CRIMSON: Color32 = Color32::from_rgb(255, 59, 92);
const COLOR_TEXT_MUTED: Color32 = Color32::from_rgb(130, 145, 165);

impl eframe::App for CyberApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let telem = self.telemetry.lock().unwrap().clone();

        // Configure cyber visuals
        let mut style = (*ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = COLOR_BG;
        style.visuals.window_fill = COLOR_BG;
        style.visuals.override_text_color = Some(Color32::WHITE);
        ctx.set_style(style);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);

            // 1. Header Bar
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("FRAMEPACER")
                        .font(FontId::monospace(17.0))
                        .strong()
                        .color(COLOR_CYAN),
                );
                ui.label(
                    RichText::new("// THE FREAK")
                        .font(FontId::monospace(14.0))
                        .color(COLOR_EMERALD),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("v0.1.0")
                            .font(FontId::monospace(11.0))
                            .color(COLOR_TEXT_MUTED),
                    );
                });
            });

            ui.add_space(2.0);

            // 2. Decorative Animated GIF Box
            let gif_texture = self.gif_player.update_and_get_texture(ctx);
            egui::Frame::canvas(ui.style())
                .fill(Color32::from_rgb(14, 17, 24))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(35, 45, 60)))
                .rounding(Rounding::same(6.0))
                .inner_margin(4.0)
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if let Some(tex) = gif_texture {
                            let tex_size = tex.size_vec2();
                            // Scale down proportionally to fit header nicely
                            let max_height = 140.0;
                            let aspect = tex_size.x / tex_size.y.max(1.0);
                            let disp_size = Vec2::new(max_height * aspect, max_height);
                            ui.image((tex.id(), disp_size));
                        } else {
                            ui.label("KAINE_CRY_ASSET_LOADING...");
                        }
                    });
                });

            ui.add_space(4.0);

            // 3. Telemetry Matrix (Main Card)
            egui::Frame::group(ui.style())
                .fill(COLOR_CARD)
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(40, 50, 70)))
                .rounding(Rounding::same(8.0))
                .inner_margin(12.0)
                .show(ui, |ui| {
                    // Digital FPS Readout
                    ui.columns(2, |cols| {
                        // Left: Target Cap
                        cols[0].vertical_centered(|ui| {
                            ui.label(
                                RichText::new("TARGET CAP")
                                    .font(FontId::monospace(10.0))
                                    .color(COLOR_TEXT_MUTED),
                            );
                            ui.label(
                                RichText::new(format!("{}", telem.current_cap))
                                    .font(FontId::monospace(36.0))
                                    .strong()
                                    .color(COLOR_CYAN),
                            );
                            ui.label(
                                RichText::new("FPS")
                                    .font(FontId::monospace(10.0))
                                    .color(COLOR_TEXT_MUTED),
                            );
                        });

                        // Right: Live ETW Measured FPS
                        cols[1].vertical_centered(|ui| {
                            ui.label(
                                RichText::new("MEASURED")
                                    .font(FontId::monospace(10.0))
                                    .color(COLOR_TEXT_MUTED),
                            );
                            let (measured_str, measured_color) = match telem.measured_fps {
                                Some(fps) => (format!("{:.1}", fps), COLOR_EMERALD),
                                None => ("--.-".to_string(), COLOR_TEXT_MUTED),
                            };
                            ui.label(
                                RichText::new(measured_str)
                                    .font(FontId::monospace(36.0))
                                    .strong()
                                    .color(measured_color),
                            );
                            ui.label(
                                RichText::new("ETW DXGI")
                                    .font(FontId::monospace(10.0))
                                    .color(COLOR_TEXT_MUTED),
                            );
                        });
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // Pacing Engine Status Chip
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("STATE:")
                                .font(FontId::monospace(11.0))
                                .color(COLOR_TEXT_MUTED),
                        );
                        let status_color = if telem.in_drop_state {
                            COLOR_AMBER
                        } else if telem.pacing_status.contains("PROB") {
                            COLOR_CYAN
                        } else if telem.pacing_status.contains("LOCKED")
                            || telem.pacing_status.contains("ACTIVE")
                        {
                            COLOR_EMERALD
                        } else {
                            COLOR_TEXT_MUTED
                        };
                        ui.label(
                            RichText::new(&telem.pacing_status)
                                .font(FontId::monospace(11.0))
                                .strong()
                                .color(status_color),
                        );
                    });

                    // Attached Game Info
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("TARGET:")
                                .font(FontId::monospace(11.0))
                                .color(COLOR_TEXT_MUTED),
                        );
                        let game_str = match (&telem.active_game_name, telem.active_game_pid) {
                            (Some(name), Some(pid)) => format!("{} [PID {}]", name, pid),
                            (Some(name), None) => name.clone(),
                            _ => "WAITING FOR HOOK...".to_string(),
                        };
                        let game_color = if telem.active_game_name.is_some() {
                            Color32::WHITE
                        } else {
                            COLOR_TEXT_MUTED
                        };
                        ui.label(
                            RichText::new(game_str)
                                .font(FontId::monospace(11.0))
                                .color(game_color),
                        );
                    });

                    // IPC Link
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("IPC:")
                                .font(FontId::monospace(11.0))
                                .color(COLOR_TEXT_MUTED),
                        );
                        let (ipc_str, ipc_color) = if telem.ipc_connected {
                            ("SHARED MEMORY [ONLINE]", COLOR_EMERALD)
                        } else {
                            ("SEARCHING IPC_v5...", COLOR_CRIMSON)
                        };
                        ui.label(
                            RichText::new(ipc_str)
                                .font(FontId::monospace(10.0))
                                .color(ipc_color),
                        );
                    });
                });

            ui.add_space(6.0);

            // 4. Controls Section
            let toggle_text = if telem.auto_enabled {
                "AUTO PACING: ACTIVE [NEVER-OFF]"
            } else {
                "AUTO PACING: PAUSED"
            };
            let toggle_color = if telem.auto_enabled {
                COLOR_EMERALD
            } else {
                COLOR_CRIMSON
            };

            let toggle_btn = egui::Button::new(
                RichText::new(toggle_text)
                    .font(FontId::monospace(13.0))
                    .strong()
                    .color(toggle_color),
            )
            .min_size(Vec2::new(ui.available_width(), 38.0))
            .stroke(Stroke::new(1.0_f32, toggle_color));

            if ui.add(toggle_btn).clicked() {
                let _ = self.command_sender.send(UiCommand::ToggleAuto);
            }

            ui.add_space(4.0);

            // Manual Nudge Buttons
            ui.horizontal(|ui| {
                let btn_down = egui::Button::new(
                    RichText::new("-5 FPS")
                        .font(FontId::monospace(11.0))
                        .color(COLOR_CYAN),
                )
                .min_size(Vec2::new(ui.available_width() * 0.5 - 4.0, 28.0));

                if ui.add(btn_down).clicked() {
                    let _ = self.command_sender.send(UiCommand::NudgeCap(-5));
                }

                let btn_up = egui::Button::new(
                    RichText::new("+5 FPS")
                        .font(FontId::monospace(11.0))
                        .color(COLOR_CYAN),
                )
                .min_size(Vec2::new(ui.available_width(), 28.0));

                if ui.add(btn_up).clicked() {
                    let _ = self.command_sender.send(UiCommand::NudgeCap(5));
                }
            });

            ui.add_space(4.0);

            // 5. Footer & Quote
            ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                ui.label(
                    RichText::new("*Glory to mankind.*")
                        .font(FontId::monospace(10.0))
                        .italics()
                        .color(Color32::from_rgb(90, 105, 125)),
                );
                ui.label(
                    RichText::new("Toggle Hotkey: Alt + Shift + A")
                        .font(FontId::monospace(9.0))
                        .color(COLOR_TEXT_MUTED),
                );
            });
        });

        // Request constant 30-60 Hz repainting for smooth cyber animation
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

pub fn run_gui(
    telemetry: Arc<Mutex<UiTelemetry>>,
    command_sender: std::sync::mpsc::Sender<UiCommand>,
) -> Result<(), eframe::Error> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([360.0, 520.0])
            .with_min_inner_size([320.0, 480.0])
            .with_always_on_top()
            .with_title("FRAMEPACER // THE FREAK"),
        ..Default::default()
    };

    eframe::run_native(
        "framepacer-the-freak",
        native_options,
        Box::new(|_cc| Ok(Box::new(CyberApp::new(telemetry, command_sender)))),
    )
}
