#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use framepacer_the_freak::analyzer::{AutoState, PacingAction, PacingAnalyzer};
use framepacer_the_freak::config::AppConfig;
use framepacer_the_freak::etw::{EtwTraceSession, get_global_recorder};
use framepacer_the_freak::hotkey::check_toggle_hotkey;
use framepacer_the_freak::ipc::FramepacerIpc;
use framepacer_the_freak::logtail::LogTailer;
use framepacer_the_freak::ui::{UiCommand, UiTelemetry, run_gui};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

fn get_process_name_by_pid(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if res.is_ok() && len > 0 {
            let full_path = String::from_utf16_lossy(&buf[..len as usize]);
            std::path::Path::new(&full_path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    }
}

struct ActiveGame {
    process_name: String,
    pid: u32,
    initial_cap: u32,
    current_cap: u32,
    limiter_enabled: bool,
    state: AutoState,
    in_drop_state: bool,
}

struct EngineController {
    config: AppConfig,
    analyzer: PacingAnalyzer,
    tailer: LogTailer,
    etw: Option<EtwTraceSession>,
    ipc: FramepacerIpc,
    auto_enabled: bool,
    active_game: Option<ActiveGame>,
    telemetry: Arc<Mutex<UiTelemetry>>,
    cmd_rx: Receiver<UiCommand>,
    hotkey_was_down: bool,
    last_ipc_retry: Instant,
}

impl EngineController {
    fn new(
        config_path: PathBuf,
        telemetry: Arc<Mutex<UiTelemetry>>,
        cmd_rx: Receiver<UiCommand>,
    ) -> Self {
        let config = AppConfig::load_or_default(&config_path);
        let auto_enabled = config.auto_enabled_at_startup;
        let analyzer = PacingAnalyzer::new(config.clone());
        let tailer = LogTailer::new(&config.framepacer_log_path, true);

        let etw = match EtwTraceSession::start("FramePacerTheFreakETW") {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "[WARN] ETW session start failed: {}. Log fallback active.",
                    e
                );
                None
            }
        };

        let mut ipc = FramepacerIpc::new();
        let _ = ipc.connect();

        Self {
            config,
            analyzer,
            tailer,
            etw,
            ipc,
            auto_enabled,
            active_game: None,
            telemetry,
            cmd_rx,
            hotkey_was_down: false,
            last_ipc_retry: Instant::now(),
        }
    }

    fn toggle_auto(&mut self) {
        self.auto_enabled = !self.auto_enabled;
        println!(
            "[INFO] Auto mode toggled: {}",
            if self.auto_enabled { "ON" } else { "OFF" }
        );

        if !self.auto_enabled {
            if let Some(game) = self.active_game.as_mut() {
                if game.current_cap != game.initial_cap {
                    println!(
                        "[INFO] Restoring initial cap {} FPS for {}",
                        game.initial_cap, game.process_name
                    );
                    let _ = self.ipc.set_target_fps(game.initial_cap);
                    game.current_cap = game.initial_cap;
                }
                game.state = AutoState::Idle;
            }
        } else if let Some(game) = self.active_game.as_mut() {
            // Never-off: Limiter remains permanently active
            let _ = self.ipc.set_limiter_enabled(true);
            game.limiter_enabled = true;
            game.state = AutoState::Settling {
                cap_fps: game.current_cap,
                settle_until: Instant::now() + Duration::from_millis(1000),
                in_drop_state: game.in_drop_state,
            };
        }
    }

    fn poll_hotkey(&mut self) {
        if check_toggle_hotkey(&mut self.hotkey_was_down) {
            self.toggle_auto();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();

        // 1. Process hotkey and UI commands
        self.poll_hotkey();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                UiCommand::ToggleAuto => self.toggle_auto(),
            }
        }

        // 2. Periodic IPC connection attempt if disconnected
        if !self.ipc.is_connected()
            && now.duration_since(self.last_ipc_retry) >= Duration::from_millis(1000)
        {
            self.last_ipc_retry = now;
            let _ = self.ipc.connect();
        }

        // Direct IPC game discovery & live manual cap tracking
        if self.ipc.is_connected() {
            if let Some(game) = self.active_game.as_mut() {
                // Continuous live sync with FramePacer's shared memory target FPS.
                // Detects instant manual cap changes applied by user in FramePacer GUI (e.g. typing 49 and Apply).
                if let Ok(live_fps) = self.ipc.get_target_fps()
                    && live_fps > 0
                    && live_fps != game.current_cap
                {
                    println!(
                        "[MANUAL SYNC] External manual cap change in FramePacer: {} -> {} FPS",
                        game.current_cap, live_fps
                    );
                    game.current_cap = live_fps;
                    game.initial_cap = live_fps;
                    game.in_drop_state = false;
                    game.state = AutoState::Settling {
                        cap_fps: live_fps,
                        settle_until: now + Duration::from_millis(1500),
                        in_drop_state: false,
                    };
                }
            }

            if self.active_game.is_none() {
                if let Ok(true) = self.ipc.is_dll_attached() {
                    let pid = self.ipc.get_attached_pid().ok().flatten().unwrap_or(0);
                    if pid > 0 {
                        let proc_name = get_process_name_by_pid(pid)
                            .unwrap_or_else(|| format!("GameProcess_{}", pid));
                        let is_excluded = self
                            .config
                            .exclude_processes
                            .iter()
                            .any(|ex| ex.eq_ignore_ascii_case(&proc_name));

                        if !is_excluded {
                            let cap = self
                                .ipc
                                .get_target_fps()
                                .ok()
                                .filter(|&f| f > 0)
                                .unwrap_or(60);

                            let _ = self.ipc.set_limiter_enabled(true);
                            println!(
                                "[IPC ATTACH] Game detected via IPC: {} (PID {}), active cap: {} FPS",
                                proc_name, pid, cap
                            );

                            self.active_game = Some(ActiveGame {
                                process_name: proc_name,
                                pid,
                                initial_cap: cap,
                                current_cap: cap,
                                limiter_enabled: true,
                                state: AutoState::Settling {
                                    cap_fps: cap,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: false,
                                },
                                in_drop_state: false,
                            });
                        }
                    }
                }
            } else if let Some(game) = &self.active_game
                && let Ok(false) = self.ipc.is_dll_attached()
            {
                println!("[IPC DETACH] Game detached via IPC: {}", game.process_name);
                let dead_pid = game.pid;
                self.active_game = None;
                if let Ok(mut rec) = get_global_recorder().lock() {
                    rec.clear_pid(dead_pid);
                }
            }
        }

        // 3. Process events from FramePacer log
        for ev in self.tailer.poll_log_events() {
            match ev.event.as_str() {
                "ipc.ready" => {
                    println!("[IPC] FramePacer IPC ready, connecting shared memory.");
                    let _ = self.ipc.connect();
                }
                "ipc.shutdown" => {
                    println!("[IPC] FramePacer IPC shutdown detected.");
                    self.ipc.disconnect();
                }
                "limiter.attached" | "limiter.pacing_started" if self.active_game.is_none() => {
                    let target = ev.get("target").unwrap_or("").to_string();
                    let pid = ev.get_u32("target_pid").unwrap_or(0);
                    let mut cap = ev.get_u32("target_fps").filter(|&f| f > 0).unwrap_or(60);
                    let _enabled = ev.get_bool("enabled").unwrap_or(true);

                    if !target.is_empty()
                        && pid > 0
                        && !self
                            .config
                            .exclude_processes
                            .iter()
                            .any(|ex| ex.eq_ignore_ascii_case(&target))
                    {
                        if let Err(e) = self.ipc.ensure_connected() {
                            eprintln!("[WARN] IPC connect failed on attach: {}", e);
                        } else if let Ok(live_fps) = self.ipc.get_target_fps()
                            && live_fps > 0
                        {
                            cap = live_fps;
                        }

                        let _ = self.ipc.set_limiter_enabled(true);

                        println!(
                            "[ATTACH] Game attached: {} (PID {}), active cap: {} FPS",
                            target, pid, cap
                        );

                        let game = ActiveGame {
                            process_name: target,
                            pid,
                            initial_cap: cap,
                            current_cap: cap,
                            limiter_enabled: true,
                            state: AutoState::Settling {
                                cap_fps: cap,
                                settle_until: now + Duration::from_millis(1500),
                                in_drop_state: false,
                            },
                            in_drop_state: false,
                        };

                        self.active_game = Some(game);
                    }
                }
                "limiter.setting_changed" => {
                    if let Some(game) = self.active_game.as_mut() {
                        if let Some(target_fps) = ev.get_u32("target_fps")
                            && target_fps > 0
                        {
                            game.current_cap = target_fps;
                            game.initial_cap = target_fps;
                            game.in_drop_state = false;
                        }
                        if let Some(en) = ev.get_bool("enabled") {
                            game.limiter_enabled = en;
                        }
                    }
                }
                "limiter.detached" | "game.process_exited" => {
                    let target = ev.get("target").unwrap_or("");
                    if let Some(game) = &self.active_game
                        && (target.is_empty() || game.process_name.eq_ignore_ascii_case(target))
                    {
                        println!("[DETACH] Game detached: {}", game.process_name);
                        let dead_pid = game.pid;
                        self.active_game = None;
                        if let Ok(mut rec) = get_global_recorder().lock() {
                            rec.clear_pid(dead_pid);
                        }
                    }
                }
                "pacing.state" => {
                    if let Some(game) = self.active_game.as_mut() {
                        if let Some(fps) = ev.get_u32("target_fps")
                            && fps > 0
                        {
                            game.current_cap = fps;
                        }
                        if let Some(en) = ev.get_bool("enabled") {
                            game.limiter_enabled = en;
                        }
                    }
                }
                _ => {}
            }
        }

        // 4. Drive auto-pacing state machine
        if self.auto_enabled {
            let mut next_state: Option<AutoState> = None;

            if let Some(game) = self.active_game.as_mut() {
                if let Some(etw) = self.etw.as_ref() {
                    etw.flush();
                }

                match game.state {
                    AutoState::Idle => {
                        let _ = self.ipc.set_limiter_enabled(true);
                        game.limiter_enabled = true;
                        next_state = Some(AutoState::Settling {
                            cap_fps: game.current_cap,
                            settle_until: now + Duration::from_millis(1500),
                            in_drop_state: game.in_drop_state,
                        });
                    }
                    AutoState::Settling {
                        cap_fps,
                        settle_until,
                        in_drop_state,
                    } => {
                        if now >= settle_until {
                            if let Ok(mut rec) = get_global_recorder().lock() {
                                rec.clear_pid(game.pid);
                            }
                            next_state = Some(AutoState::PacingActive {
                                cap_fps,
                                stable_since: now,
                                in_drop_state,
                            });
                        }
                    }
                    AutoState::PacingActive {
                        cap_fps,
                        stable_since,
                        in_drop_state,
                    } => {
                        let recorder = get_global_recorder();
                        let intervals = if let Ok(rec) = recorder.lock() {
                            rec.get_frame_intervals(game.pid, Duration::from_secs_f64(2.5))
                        } else {
                            Vec::new()
                        };

                        let action = self.analyzer.evaluate_intervals(
                            cap_fps,
                            &intervals,
                            now,
                            stable_since,
                            in_drop_state,
                        );

                        match action {
                            PacingAction::KeepCap => {}
                            PacingAction::StepDown { from_fps, to_fps } => {
                                println!(
                                    "[STEP] Pacing struggling -> lowering cap {} -> {} FPS",
                                    from_fps, to_fps
                                );
                                let _ = self.ipc.set_target_fps(to_fps);
                                game.current_cap = to_fps;
                                let still_in_drop = to_fps < game.initial_cap;
                                game.in_drop_state = still_in_drop;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: to_fps,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: still_in_drop,
                                });
                            }
                            PacingAction::JumpTo { from_fps, to_fps } => {
                                println!(
                                    "[CUTSCENE] Cutscene drop -> snapping cap {} -> {} FPS",
                                    from_fps, to_fps
                                );
                                let _ = self.ipc.set_target_fps(to_fps);
                                game.current_cap = to_fps;
                                game.in_drop_state = true;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: to_fps,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: true,
                                });
                            }
                            PacingAction::MeasureNative => {
                                println!(
                                    "[BENCHMARK] 30s benchmark cycle -> disengaging limiter for 1.0s to measure native FPS"
                                );
                                let _ = self.ipc.set_limiter_enabled(false);
                                game.limiter_enabled = false;
                                if let Ok(mut rec) = get_global_recorder().lock() {
                                    rec.clear_pid(game.pid);
                                }
                                next_state = Some(AutoState::MeasuringNative {
                                    started_at: now,
                                    previous_cap: cap_fps,
                                });
                            }
                        }
                    }
                    AutoState::MeasuringNative {
                        started_at,
                        previous_cap,
                    } => {
                        let elapsed = now.duration_since(started_at);
                        if elapsed >= Duration::from_secs_f64(self.config.measure_duration_secs) {
                            let recorder = get_global_recorder();
                            let measured_fps = recorder.lock().ok().and_then(|rec| {
                                rec.get_current_fps(
                                    game.pid,
                                    Duration::from_secs_f64(self.config.measure_duration_secs),
                                )
                            });

                            let new_cap = if let Some(fps) = measured_fps {
                                self.config.clamp_and_round_fps(fps)
                            } else {
                                previous_cap
                            };

                            println!(
                                "[BENCHMARK COMPLETE] Measured native: {:?} FPS -> setting cap {} FPS and re-engaging limiter",
                                measured_fps, new_cap
                            );

                            let _ = self.ipc.set_target_fps(new_cap);
                            let _ = self.ipc.set_limiter_enabled(true);
                            game.current_cap = new_cap;
                            game.limiter_enabled = true;
                            game.in_drop_state = false;
                            next_state = Some(AutoState::Settling {
                                cap_fps: new_cap,
                                settle_until: now + Duration::from_millis(1000),
                                in_drop_state: false,
                            });
                        }
                    }
                }
            }

            if let Some(ns) = next_state
                && let Some(game) = self.active_game.as_mut()
            {
                game.state = ns;
            }
        }

        // 5. Synchronize UI telemetry
        let measured_fps = self.active_game.as_ref().and_then(|g| {
            let recorder = get_global_recorder();
            let rec = recorder.lock().ok()?;
            rec.get_current_fps(g.pid, Duration::from_secs(2))
        });

        let is_measuring = self
            .active_game
            .as_ref()
            .map(|g| matches!(g.state, AutoState::MeasuringNative { .. }))
            .unwrap_or(false);

        let (status_str, in_drop) = if !self.auto_enabled {
            ("PAUSED".to_string(), false)
        } else if let Some(game) = &self.active_game {
            match game.state {
                AutoState::Idle => ("STANDBY".to_string(), false),
                AutoState::MeasuringNative { .. } => ("BENCHMARKING NATIVE".to_string(), false),
                AutoState::Settling {
                    in_drop_state: true,
                    ..
                } => ("CUTSCENE LOCKING (30)".to_string(), true),
                AutoState::Settling { .. } => ("SETTLING CADENCE".to_string(), false),
                AutoState::PacingActive {
                    in_drop_state: true,
                    ..
                } => ("CUTSCENE SNAP (30 FPS)".to_string(), true),
                AutoState::PacingActive { .. } => ("PACING LOCKED".to_string(), false),
            }
        } else {
            ("WAITING FOR GAME...".to_string(), false)
        };

        if let Ok(mut telem) = self.telemetry.lock() {
            telem.auto_enabled = self.auto_enabled;
            telem.active_game_name = self.active_game.as_ref().map(|g| g.process_name.clone());
            telem.active_game_pid = self.active_game.as_ref().map(|g| g.pid);
            telem.current_cap = self
                .active_game
                .as_ref()
                .map(|g| g.current_cap)
                .unwrap_or_else(|| self.ipc.get_target_fps().unwrap_or(60));
            telem.measured_fps = measured_fps;
            telem.is_measuring_native = is_measuring;
            telem.pacing_status = status_str;
            telem.ipc_connected = self.ipc.is_connected();
            telem.in_drop_state = in_drop;
            telem.etw_active = self.etw.is_some();
        }
    }

    fn shutdown(&mut self) {
        println!("[INFO] Shutting down FramePacer The Freak...");
        if let Some(game) = self.active_game.take()
            && game.current_cap != game.initial_cap
        {
            println!(
                "[INFO] Restoring initial cap {} FPS for {}",
                game.initial_cap, game.process_name
            );
            let _ = self.ipc.set_target_fps(game.initial_cap);
        }
        self.ipc.disconnect();
        if let Some(mut etw) = self.etw.take() {
            etw.stop();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== FRAMEPACER // THE FREAK ===");

    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir = exe_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let data_config = config_dir.join("data").join("config.toml");
    let config_path = if data_config.exists() {
        data_config
    } else if config_dir.join("config.toml").exists() {
        config_dir.join("config.toml")
    } else if config_dir.join("data").is_dir() {
        data_config
    } else {
        config_dir.join("config.toml")
    };

    let telemetry = Arc::new(Mutex::new(UiTelemetry::default()));
    let (cmd_tx, cmd_rx) = channel::<UiCommand>();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();
    let engine_telemetry = telemetry.clone();

    // Spawn background pacing engine thread (running at 100ms cadence)
    let engine_thread = std::thread::spawn(move || {
        let mut engine = EngineController::new(config_path, engine_telemetry, cmd_rx);
        println!("[READY] Pacing engine initialized.");

        while engine_running.load(Ordering::Relaxed) {
            engine.tick();
            std::thread::sleep(Duration::from_millis(100));
        }

        engine.shutdown();
        println!("[EXIT] Pacing engine cleanly stopped.");
    });

    // Run Luxury Cyber GUI on the main thread (blocking until window closed)
    let gui_res = run_gui(telemetry, cmd_tx);

    // Stop background engine
    running.store(false, Ordering::Relaxed);
    let _ = engine_thread.join();

    if let Err(e) = gui_res {
        eprintln!("[ERROR] GUI error: {}", e);
    }

    Ok(())
}
