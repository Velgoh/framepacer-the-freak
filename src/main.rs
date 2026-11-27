#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use framepacer_the_freak::analyzer::{AutoState, CeilingTracker, PacingAction, PacingAnalyzer};
use framepacer_the_freak::config::{AppConfig, PacingMode};
use framepacer_the_freak::etw::{EtwTraceSession, get_global_recorder};
use framepacer_the_freak::hotkey::check_toggle_hotkey;
use framepacer_the_freak::ipc::FramepacerIpc;
use framepacer_the_freak::logger::{
    init_logger, is_process_elevated, log_error, log_info, log_warn,
};
use framepacer_the_freak::logtail::LogTailer;
use framepacer_the_freak::telemetry::{HardwareLoad, HardwareMonitor};
use framepacer_the_freak::ui::{UiCommand, UiTelemetry, run_gui};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible,
};
use windows::core::BOOL;

fn get_process_exe_path_by_pid(pid: u32) -> Option<PathBuf> {
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
            Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
        } else {
            None
        }
    }
}

fn get_process_name_by_pid(pid: u32) -> Option<String> {
    get_process_exe_path_by_pid(pid)?
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

struct WindowSearchState {
    target_pid: u32,
    visible_top_windows: u32,
    non_iconic_windows: u32,
}

unsafe extern "system" fn enum_proc_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut WindowSearchState) };
    let mut win_pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut win_pid));
    }
    if win_pid != state.target_pid {
        return BOOL(1);
    }
    unsafe {
        let visible = IsWindowVisible(hwnd).as_bool();
        let unowned = GetWindow(hwnd, GW_OWNER).unwrap_or_default().is_invalid();
        if visible && unowned {
            state.visible_top_windows += 1;
            if !IsIconic(hwnd).as_bool() {
                state.non_iconic_windows += 1;
            }
        }
    }
    BOOL(1)
}

fn is_process_minimized(pid: u32) -> bool {
    if pid <= 4 {
        return false;
    }
    unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_invalid() {
            let mut fg_pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut fg_pid));
            if fg_pid == pid {
                return false;
            }
        }
        let mut state = WindowSearchState {
            target_pid: pid,
            visible_top_windows: 0,
            non_iconic_windows: 0,
        };
        let _ = EnumWindows(
            Some(enum_proc_windows_cb),
            LPARAM((&mut state as *mut WindowSearchState) as isize),
        );
        state.visible_top_windows > 0 && state.non_iconic_windows == 0
    }
}

fn fmt_opt_pct(val: Option<f64>) -> String {
    match val {
        Some(v) => format!("{:.0}%", v),
        None => "n/a".to_string(),
    }
}

struct ActiveGame {
    process_name: String,
    pid: u32,
    attached_at: Instant,
    initial_cap: u32,
    initial_limiter_enabled: bool,
    current_cap: u32,
    limiter_enabled: bool,
    state: AutoState,
    in_drop_state: bool,
    is_backgrounded: bool,
}

struct EngineController {
    config: AppConfig,
    analyzer: PacingAnalyzer,
    tailer: LogTailer,
    etw: Option<EtwTraceSession>,
    etw_error: Option<String>,
    is_elevated: bool,
    ipc: FramepacerIpc,
    hw_monitor: HardwareMonitor,
    last_hw_load: HardwareLoad,
    ceiling: CeilingTracker,
    auto_enabled: bool,
    active_game: Option<ActiveGame>,
    telemetry: Arc<Mutex<UiTelemetry>>,
    cmd_rx: Receiver<UiCommand>,
    hotkey_was_down: bool,
    last_ipc_retry: Instant,
    last_etw_flush: Instant,
    last_status_log: Instant,
    last_ipc_ft_idx: u32,
    last_ipc_ft_change: Instant,
    window_start_widx: u32,
    probe_start_widx: u32,
    logged_ipc_fallback: bool,
}

impl EngineController {
    fn new(
        config_path: PathBuf,
        telemetry: Arc<Mutex<UiTelemetry>>,
        cmd_rx: Receiver<UiCommand>,
    ) -> Self {
        let is_elevated = is_process_elevated();
        let config = AppConfig::load_or_default(&config_path);
        let auto_enabled = config.auto_enabled_at_startup;
        let analyzer = PacingAnalyzer::new(config.clone());
        let tailer = LogTailer::new(&config.framepacer_log_path, true);
        let hw_monitor = HardwareMonitor::new();
        let pdh_ok = hw_monitor.pdh_available();

        log_info(format!(
            "[CONFIG] mode={:?} fps={}..{} step={} gpu_scale_up={:.0}% gpu_hold={:.0}% cpu_max={:.0}%",
            config.pacing_mode,
            config.min_fps,
            config.max_fps,
            config.step_fps,
            config.scale_up_gpu_util_pct,
            config.hold_gpu_util_pct,
            config.max_cpu_thread_util_pct
        ));

        let (etw, etw_error) = match EtwTraceSession::start("FramePacerCompanionETW") {
            Ok(s) => {
                log_info("ETW realtime session started (DXGI + D3D9).");
                (Some(s), None)
            }
            Err(e) => {
                log_warn(format!(
                    "ETW session start failed (elevated={}): {}",
                    is_elevated, e
                ));
                (None, Some(e))
            }
        };

        log_info(format!(
            "Telemetry ready: pacing_mode={:?}, pdh_gpu_counters={}",
            config.pacing_mode, pdh_ok
        ));

        let mut ipc = FramepacerIpc::new();
        match ipc.connect() {
            Ok(()) => log_info("Connected to FramePacer IPC (Local\\FramepacerIPC_v5)."),
            Err(e) => log_info(format!("Waiting for FramePacer IPC: {}", e)),
        }

        let now = Instant::now();
        let mut controller = Self {
            config,
            analyzer,
            tailer,
            etw,
            etw_error,
            is_elevated,
            ipc,
            hw_monitor,
            last_hw_load: HardwareLoad {
                pdh_available: pdh_ok,
                ..Default::default()
            },
            ceiling: CeilingTracker::default(),
            auto_enabled,
            active_game: None,
            telemetry,
            cmd_rx,
            hotkey_was_down: false,
            last_ipc_retry: now,
            last_etw_flush: now,
            last_status_log: now,
            last_ipc_ft_idx: 0,
            last_ipc_ft_change: now,
            window_start_widx: 0,
            probe_start_widx: 0,
            logged_ipc_fallback: false,
        };
        controller.sync_framepacer_log_path();
        controller
    }

    fn mark_ipc_window_start(&mut self) {
        self.window_start_widx = self
            .ipc
            .read_frametime_ring(1)
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    fn sync_framepacer_log_path(&mut self) {
        if !self.tailer.path().exists()
            && self.ipc.is_alive()
            && let Ok(Some(owner_pid)) = self.ipc.get_owner_pid()
            && let Some(exe_path) = get_process_exe_path_by_pid(owner_pid)
            && let Some(dir) = exe_path.parent()
        {
            let candidate = dir.join("framepacer.log");
            if candidate != self.tailer.path() {
                self.tailer.set_path(candidate.clone(), true);
                log_info(format!(
                    "[LOG] Auto-detected FramePacer log path: {}",
                    candidate.display()
                ));
            }
        }
    }

    fn attach_game(
        &mut self,
        proc_name: String,
        pid: u32,
        fallback_cap: u32,
        fallback_enabled: bool,
        now: Instant,
    ) {
        self.last_ipc_ft_idx = 0;
        self.last_ipc_ft_change = now;
        self.window_start_widx = 0;
        self.probe_start_widx = 0;
        self.logged_ipc_fallback = false;
        self.last_status_log = now;
        self.hw_monitor.reset();
        self.ceiling.clear();
        self.last_hw_load = HardwareLoad {
            pdh_available: self.hw_monitor.pdh_available(),
            ..Default::default()
        };

        let (live_enabled, live_fps) = if self.ipc.is_alive() {
            self.ipc
                .get_state()
                .unwrap_or((fallback_enabled, fallback_cap))
        } else {
            (fallback_enabled, fallback_cap)
        };

        let (state, current_cap, limiter_enabled) = if !self.auto_enabled {
            log_info(format!(
                "[ATTACH] {} (PID {}), auto=OFF, cap={} FPS, limiter={}",
                proc_name, pid, live_fps, live_enabled
            ));
            (AutoState::Idle, live_fps, live_enabled)
        } else if live_fps > 0 {
            let _ = self.ipc.set_limiter_enabled(true);
            log_info(format!(
                "[ATTACH] {} (PID {}), auto=ON, mode={:?}, active cap={} FPS -> settling",
                proc_name, pid, self.config.pacing_mode, live_fps
            ));
            (
                AutoState::Settling {
                    cap_fps: live_fps,
                    settle_until: now + Duration::from_millis(1500),
                    in_drop_state: false,
                },
                live_fps,
                true,
            )
        } else if self.config.pacing_mode == PacingMode::NeverOff {
            let start_cap = 60u32.clamp(self.config.min_fps, self.config.max_fps);
            let _ = self.ipc.set_target_fps(start_cap);
            let _ = self.ipc.set_limiter_enabled(true);
            log_info(format!(
                "[ATTACH] {} (PID {}), auto=ON, mode=NeverOff, no initial cap -> starting at {} FPS",
                proc_name, pid, start_cap
            ));
            (
                AutoState::Settling {
                    cap_fps: start_cap,
                    settle_until: now + Duration::from_millis(1200),
                    in_drop_state: false,
                },
                start_cap,
                true,
            )
        } else {
            if let Ok(mut rec) = get_global_recorder().lock() {
                rec.clear_pid(pid);
            }
            let _ = self.ipc.set_limiter_enabled(false);
            log_info(format!(
                "[ATTACH] {} (PID {}), auto=ON, mode=Pulse, no initial cap (0) -> measuring native FPS",
                proc_name, pid
            ));
            (
                AutoState::MeasuringNative {
                    started_at: now,
                    previous_cap: 0,
                },
                0,
                false,
            )
        };

        self.active_game = Some(ActiveGame {
            process_name: proc_name,
            pid,
            attached_at: now,
            initial_cap: live_fps,
            initial_limiter_enabled: live_enabled,
            current_cap,
            limiter_enabled,
            state,
            in_drop_state: false,
            is_backgrounded: false,
        });
    }

    fn clear_active_game(&mut self, reason: &str) {
        self.last_ipc_ft_idx = 0;
        self.window_start_widx = 0;
        self.probe_start_widx = 0;
        self.logged_ipc_fallback = false;
        self.hw_monitor.reset();
        self.ceiling.clear();
        self.last_hw_load = HardwareLoad {
            pdh_available: self.hw_monitor.pdh_available(),
            ..Default::default()
        };
        if let Some(game) = self.active_game.take() {
            log_info(format!(
                "[DETACH] {} (PID {}): {}",
                game.process_name, game.pid, reason
            ));
            if let Ok(mut rec) = get_global_recorder().lock() {
                rec.clear_pid(game.pid);
            }
        }
    }

    fn read_ipc_frametime_ring_fresh(
        &mut self,
        pid: u32,
        max_samples: usize,
        now: Instant,
    ) -> (u32, Vec<f64>) {
        if !self.ipc.is_alive() {
            return (0, Vec::new());
        }
        if let Ok((widx, samples)) = self.ipc.read_frametime_ring(max_samples) {
            if widx != self.last_ipc_ft_idx {
                self.last_ipc_ft_idx = widx;
                self.last_ipc_ft_change = now;
            }
            if widx > 0
                && now.duration_since(self.last_ipc_ft_change) <= Duration::from_secs(2)
                && !samples.is_empty()
            {
                if !self.logged_ipc_fallback {
                    self.logged_ipc_fallback = true;
                    log_info(format!(
                        "[METRICS] FramePacer IPC frametime ring active for PID {}",
                        pid
                    ));
                }
                return (widx, samples);
            }
        }
        (0, Vec::new())
    }

    fn get_live_intervals(
        &mut self,
        pid: u32,
        window: Duration,
        now: Instant,
    ) -> (Vec<f64>, &'static str) {
        let etw_intervals = if let Ok(rec) = get_global_recorder().lock() {
            rec.get_frame_intervals_recent(pid, window)
        } else {
            Vec::new()
        };
        let (widx, samples) = self.read_ipc_frametime_ring_fresh(pid, 120, now);
        let ipc_intervals = if widx == 0 || samples.is_empty() {
            Vec::new()
        } else if self.window_start_widx > 0 {
            let new_count =
                (widx.wrapping_sub(self.window_start_widx) as usize).min(samples.len());
            if new_count > 2 {
                samples[samples.len() - new_count + 1..].to_vec()
            } else {
                Vec::new()
            }
        } else {
            samples
        };

        if ipc_intervals.len() >= 5 && ipc_intervals.len() >= etw_intervals.len() {
            (ipc_intervals, "ipc")
        } else if etw_intervals.len() >= 5 {
            (etw_intervals, "etw")
        } else {
            (Vec::new(), "none")
        }
    }

    fn get_probe_intervals(
        &mut self,
        pid: u32,
        window: Duration,
        now: Instant,
    ) -> (Vec<f64>, &'static str) {
        let etw_intervals = if let Ok(rec) = get_global_recorder().lock() {
            rec.get_frame_intervals_recent(pid, window)
        } else {
            Vec::new()
        };
        let (widx, samples) = self.read_ipc_frametime_ring_fresh(pid, 120, now);
        let ipc_intervals = if widx == 0 || samples.is_empty() {
            Vec::new()
        } else if self.probe_start_widx > 0 {
            let new_count =
                (widx.wrapping_sub(self.probe_start_widx) as usize).min(samples.len());
            if new_count > 3 {
                let start = samples.len() - new_count + 2;
                samples[start..].to_vec()
            } else {
                Vec::new()
            }
        } else {
            let take = samples.len().min(20);
            samples[samples.len() - take..].to_vec()
        };

        if ipc_intervals.len() >= 5 && ipc_intervals.len() >= etw_intervals.len() {
            (ipc_intervals, "ipc")
        } else if etw_intervals.len() >= 5 {
            (etw_intervals, "etw")
        } else {
            (Vec::new(), "none")
        }
    }

    fn get_live_fps(
        &mut self,
        pid: u32,
        window: Duration,
        warmup: Duration,
        trimmed: bool,
        now: Instant,
    ) -> Option<f64> {
        let etw_fps = get_global_recorder().lock().ok().and_then(|rec| {
            if trimmed {
                rec.get_trimmed_fps_recent(pid, window, warmup)
            } else {
                rec.get_current_fps_recent(pid, window)
            }
        });
        if etw_fps.is_some() {
            return etw_fps;
        }

        let mut samples = self.read_ipc_frametime_ring_fresh(pid, 120, now).1;
        if trimmed && samples.len() > 20 {
            samples.drain(..15);
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let trim = ((samples.len() as f64) * 0.05).floor() as usize;
            let slice = if trim * 2 < samples.len() {
                &samples[trim..samples.len() - trim]
            } else {
                &samples[..]
            };
            let total_ms: f64 = slice.iter().sum();
            if total_ms > 0.0 && !slice.is_empty() {
                return Some(1000.0 / (total_ms / slice.len() as f64));
            }
        } else if samples.len() >= 3 {
            let total_ms: f64 = samples.iter().sum();
            if total_ms > 0.0 {
                return Some(1000.0 / (total_ms / samples.len() as f64));
            }
        }
        None
    }

    fn toggle_auto(&mut self) {
        self.auto_enabled = !self.auto_enabled;
        log_info(format!(
            "Auto mode toggled: {}",
            if self.auto_enabled { "ON" } else { "OFF" }
        ));

        let now = Instant::now();
        if !self.auto_enabled {
            if let Some(game) = self.active_game.as_mut() {
                if game.initial_cap > 0 && game.current_cap != game.initial_cap {
                    log_info(format!(
                        "Restoring initial cap {} FPS for {}",
                        game.initial_cap, game.process_name
                    ));
                    let _ = self.ipc.set_target_fps(game.initial_cap);
                }
                let _ = self.ipc.set_limiter_enabled(game.initial_limiter_enabled);
                game.current_cap = game.initial_cap;
                game.limiter_enabled = game.initial_limiter_enabled;
                game.state = AutoState::Idle;
                self.ceiling.clear();
            }
        } else if let Some(game) = self.active_game.as_mut() {
            if game.current_cap > 0 {
                let _ = self.ipc.set_limiter_enabled(true);
                game.limiter_enabled = true;
                game.state = AutoState::Settling {
                    cap_fps: game.current_cap,
                    settle_until: now + Duration::from_millis(1000),
                    in_drop_state: game.in_drop_state,
                };
            } else if self.config.pacing_mode == PacingMode::NeverOff {
                let start_cap = 60u32.clamp(self.config.min_fps, self.config.max_fps);
                let _ = self.ipc.set_target_fps(start_cap);
                let _ = self.ipc.set_limiter_enabled(true);
                game.current_cap = start_cap;
                game.limiter_enabled = true;
                game.state = AutoState::Settling {
                    cap_fps: start_cap,
                    settle_until: now + Duration::from_millis(1200),
                    in_drop_state: false,
                };
            } else {
                if let Ok(mut rec) = get_global_recorder().lock() {
                    rec.clear_pid(game.pid);
                }
                let _ = self.ipc.set_limiter_enabled(false);
                game.limiter_enabled = false;
                game.state = AutoState::MeasuringNative {
                    started_at: now,
                    previous_cap: 0,
                };
            }
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

        // 2. Verify live IPC health & reconnect when FramePacer starts/restarts
        if self.ipc.is_connected() && !self.ipc.is_alive() {
            log_warn("[IPC] FramePacer section closed/restarted; disconnecting.");
            self.ipc.disconnect();
            self.clear_active_game("IPC section closed");
        }

        if !self.ipc.is_connected()
            && now.duration_since(self.last_ipc_retry) >= Duration::from_millis(1000)
        {
            self.last_ipc_retry = now;
            if self.ipc.connect().is_ok() {
                log_info("[IPC] Connected to FramePacer shared memory.");
                self.sync_framepacer_log_path();
            }
        }

        // Direct IPC game discovery & live manual cap tracking
        if self.ipc.is_alive() {
            if let Some(game) = self.active_game.as_mut() {
                // Continuous live sync with FramePacer's shared memory target FPS.
                // Skip while MeasuringNative or ProbingUp so we don't fight our own probes.
                if !matches!(
                    game.state,
                    AutoState::MeasuringNative { .. } | AutoState::ProbingUp { .. }
                ) && let Ok(live_fps) = self.ipc.get_target_fps()
                    && live_fps > 0
                    && live_fps != game.current_cap
                {
                    log_info(format!(
                        "[MANUAL SYNC] External cap change in FramePacer: {} -> {} FPS",
                        game.current_cap, live_fps
                    ));
                    game.current_cap = live_fps;
                    game.initial_cap = live_fps;
                    game.in_drop_state = false;
                    self.ceiling.clear();
                    game.state = if self.auto_enabled {
                        AutoState::Settling {
                            cap_fps: live_fps,
                            settle_until: now + Duration::from_millis(1500),
                            in_drop_state: false,
                        }
                    } else {
                        AutoState::Idle
                    };
                }
            }

            let live_pid = self.ipc.get_attached_pid().ok().flatten();
            if let (Some(game), Some(new_pid)) = (self.active_game.as_ref(), live_pid)
                && new_pid != game.pid
            {
                self.clear_active_game("switched to new target PID");
            }

            if self.active_game.is_none() {
                if let Some(pid) = live_pid {
                    let proc_name = get_process_name_by_pid(pid)
                        .unwrap_or_else(|| format!("GameProcess_{}", pid));
                    let is_excluded = self
                        .config
                        .exclude_processes
                        .iter()
                        .any(|ex| ex.eq_ignore_ascii_case(&proc_name));

                    if !is_excluded {
                        self.attach_game(proc_name, pid, 0, false, now);
                    }
                }
            } else if let Some(game) = self.active_game.as_ref()
                && (self.ipc.is_dll_attached().ok() == Some(false)
                    || (live_pid.is_none()
                        && now.duration_since(game.attached_at) > Duration::from_secs(1)))
            {
                self.clear_active_game("DLL detached via IPC");
            }
        }

        // 3. Process events from FramePacer log
        for ev in self.tailer.poll_log_events() {
            match ev.event.as_str() {
                "ipc.ready" => {
                    log_info("[LOG] FramePacer ipc.ready detected, connecting shared memory.");
                    if self.ipc.connect().is_ok() {
                        self.sync_framepacer_log_path();
                    }
                }
                "ipc.shutdown" => {
                    log_info("[LOG] FramePacer ipc.shutdown detected.");
                    self.ipc.disconnect();
                    self.clear_active_game("FramePacer shutdown");
                }
                "limiter.attached" | "limiter.pacing_started" if self.active_game.is_none() => {
                    let target = ev.get("target").unwrap_or("").to_string();
                    let pid = ev.get_u32("target_pid").unwrap_or(0);
                    let cap = ev.get_u32("target_fps").unwrap_or(0);
                    let enabled = ev.get_bool("enabled").unwrap_or(false);

                    if !target.is_empty()
                        && pid > 0
                        && !self
                            .config
                            .exclude_processes
                            .iter()
                            .any(|ex| ex.eq_ignore_ascii_case(&target))
                    {
                        let _ = self.ipc.ensure_connected();
                        self.attach_game(target, pid, cap, enabled, now);
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
                            self.ceiling.clear();
                        }
                        if let Some(en) = ev.get_bool("enabled") {
                            game.limiter_enabled = en;
                        }
                    }
                }
                "limiter.detached" | "game.process_exited" => {
                    let target = ev.get("target").unwrap_or("");
                    let matches_active = self
                        .active_game
                        .as_ref()
                        .map(|g| target.is_empty() || g.process_name.eq_ignore_ascii_case(target))
                        .unwrap_or(false);
                    if matches_active {
                        self.clear_active_game("log event");
                    }
                }
                "pacing.state" => {
                    if let Some(game) = self.active_game.as_mut()
                        && !matches!(
                            game.state,
                            AutoState::MeasuringNative { .. } | AutoState::ProbingUp { .. }
                        )
                    {
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

        // Poll GPU & CPU headroom telemetry and flush ETW while a game is active
        if let Some(game) = self.active_game.as_ref() {
            self.last_hw_load = self.hw_monitor.sample(game.pid, now);
            self.ceiling
                .maybe_clear_on_load_drop(self.last_hw_load.gpu_util_pct);

            if now.duration_since(self.last_etw_flush) >= Duration::from_millis(250)
                && let Some(etw) = self.etw.as_ref()
            {
                etw.flush();
                self.last_etw_flush = now;
            }
        }

        // Pause auto-pacing while game is minimized or FramePacer reports inactive (0x034 == 2)
        let mut pause_state_machine = false;
        let ipc_inactive =
            self.ipc.is_alive() && self.ipc.is_process_active().ok() == Some(false);
        if let Some(game) = self.active_game.as_mut() {
            let minimized = is_process_minimized(game.pid);
            let backgrounded = ipc_inactive || minimized;
            if backgrounded {
                if !game.is_backgrounded {
                    log_info(format!(
                        "[BACKGROUND] {} inactive/minimized -> pausing cap adjustments at {} FPS",
                        game.process_name, game.current_cap
                    ));
                }
                game.is_backgrounded = true;
                match game.state {
                    AutoState::ProbingUp { previous_cap, .. } if previous_cap > 0 => {
                        let _ = self.ipc.set_target_fps(previous_cap);
                        game.current_cap = previous_cap;
                    }
                    AutoState::MeasuringNative { previous_cap, .. } if previous_cap > 0 => {
                        let _ = self.ipc.set_target_fps(previous_cap);
                        let _ = self.ipc.set_limiter_enabled(true);
                        game.current_cap = previous_cap;
                        game.limiter_enabled = true;
                    }
                    _ => {}
                }
                if !matches!(game.state, AutoState::Idle) {
                    game.state = AutoState::Settling {
                        cap_fps: game.current_cap,
                        settle_until: now + Duration::from_millis(1200),
                        in_drop_state: game.in_drop_state,
                    };
                }
                pause_state_machine = true;
            } else if game.is_backgrounded {
                game.is_backgrounded = false;
                log_info(format!(
                    "[FOREGROUND] {} resumed -> settling at {} FPS",
                    game.process_name, game.current_cap
                ));
                if !matches!(game.state, AutoState::Idle) {
                    game.state = AutoState::Settling {
                        cap_fps: game.current_cap,
                        settle_until: now + Duration::from_millis(1200),
                        in_drop_state: game.in_drop_state,
                    };
                }
            }
        }

        // 4. Drive auto-pacing state machine
        if self.auto_enabled && !pause_state_machine {
            let mut next_state: Option<AutoState> = None;

            if let Some(game) = self.active_game.as_mut() {
                match game.state {
                    AutoState::Idle => {
                        if game.current_cap > 0 {
                            let _ = self.ipc.set_limiter_enabled(true);
                            game.limiter_enabled = true;
                            next_state = Some(AutoState::Settling {
                                cap_fps: game.current_cap,
                                settle_until: now + Duration::from_millis(1500),
                                in_drop_state: game.in_drop_state,
                            });
                        } else if self.config.pacing_mode == PacingMode::NeverOff {
                            let start_cap = 60u32.clamp(self.config.min_fps, self.config.max_fps);
                            let _ = self.ipc.set_target_fps(start_cap);
                            let _ = self.ipc.set_limiter_enabled(true);
                            game.current_cap = start_cap;
                            game.limiter_enabled = true;
                            next_state = Some(AutoState::Settling {
                                cap_fps: start_cap,
                                settle_until: now + Duration::from_millis(1200),
                                in_drop_state: false,
                            });
                        } else {
                            if let Ok(mut rec) = get_global_recorder().lock() {
                                rec.clear_pid(game.pid);
                            }
                            let _ = self.ipc.set_limiter_enabled(false);
                            game.limiter_enabled = false;
                            next_state = Some(AutoState::MeasuringNative {
                                started_at: now,
                                previous_cap: 0,
                            });
                        }
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
                            if cap_fps == 0 {
                                let _ = self.ipc.set_limiter_enabled(false);
                                game.limiter_enabled = false;
                                next_state = Some(AutoState::MeasuringNative {
                                    started_at: now,
                                    previous_cap: 0,
                                });
                            } else {
                                next_state = Some(AutoState::PacingActive {
                                    cap_fps,
                                    stable_since: now,
                                    in_drop_state,
                                });
                            }
                        }
                    }
                    AutoState::PacingActive {
                        cap_fps,
                        stable_since,
                        in_drop_state,
                    } => {
                        let pid = game.pid;
                        let (intervals, interval_src) =
                            self.get_live_intervals(pid, Duration::from_secs_f64(2.5), now);
                        let hw = self.last_hw_load.clone();

                        let action = match self.config.pacing_mode {
                            PacingMode::NeverOff => self.analyzer.evaluate_never_off(
                                cap_fps,
                                &intervals,
                                &hw,
                                &self.ceiling,
                                now,
                                stable_since,
                                in_drop_state,
                            ),
                            PacingMode::Pulse => self.analyzer.evaluate_intervals(
                                cap_fps,
                                &intervals,
                                now,
                                stable_since,
                                in_drop_state,
                            ),
                        };

                        let game = self.active_game.as_mut().unwrap();
                        match action {
                            PacingAction::KeepCap => {
                                if now.duration_since(stable_since) >= Duration::from_secs(6)
                                    && intervals.len() >= 15
                                    && self.analyzer.is_cap_achieved(cap_fps, &intervals)
                                {
                                    self.ceiling.note_achieved_cap(cap_fps);
                                }
                            }
                            PacingAction::StepDown { from_fps, to_fps } => {
                                let avg_fps = PacingAnalyzer::summarize_intervals(&intervals)
                                    .unwrap_or(0.0);
                                let _ = self.ipc.set_target_fps(to_fps);
                                self.ceiling.record_step_down_ceiling(
                                    from_fps,
                                    to_fps,
                                    hw.gpu_util_pct,
                                    now,
                                );
                                log_info(format!(
                                    "[STEP] Pacing struggling -> lowering cap {} -> {} FPS (avg_fps={:.1}, samples={}, src={}, gpu={}, cpu_thr={}, backoff={:.0}s)",
                                    from_fps,
                                    to_fps,
                                    avg_fps,
                                    intervals.len(),
                                    interval_src,
                                    fmt_opt_pct(hw.gpu_util_pct),
                                    fmt_opt_pct(hw.max_cpu_thread_pct),
                                    self.ceiling.backoff_secs
                                ));
                                game.current_cap = to_fps;
                                let still_in_drop = game.in_drop_state || to_fps <= 35;
                                game.in_drop_state = still_in_drop;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: to_fps,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: still_in_drop,
                                });
                            }
                            PacingAction::JumpTo { from_fps, to_fps } => {
                                let avg_fps = PacingAnalyzer::summarize_intervals(&intervals)
                                    .unwrap_or(0.0);
                                let hw_pegged = hw
                                    .gpu_util_pct
                                    .is_some_and(|g| g >= self.config.hold_gpu_util_pct)
                                    || hw
                                        .max_cpu_thread_pct
                                        .is_some_and(|c| c >= self.config.max_cpu_thread_util_pct);
                                let is_cutscene = to_fps <= 35
                                    || (!hw_pegged && self.ceiling.session_high_cap > to_fps);
                                if is_cutscene {
                                    log_info(format!(
                                        "[CUTSCENE] Cutscene drop -> snapping cap {} -> {} FPS (avg_fps={:.1}, samples={}, src={}, gpu={}, cpu_thr={})",
                                        from_fps,
                                        to_fps,
                                        avg_fps,
                                        intervals.len(),
                                        interval_src,
                                        fmt_opt_pct(hw.gpu_util_pct),
                                        fmt_opt_pct(hw.max_cpu_thread_pct)
                                    ));
                                } else {
                                    log_info(format!(
                                        "[SNAP] Heavy load drop -> snapping cap {} -> {} FPS (avg_fps={:.1}, samples={}, src={}, gpu={}, cpu_thr={})",
                                        from_fps,
                                        to_fps,
                                        avg_fps,
                                        intervals.len(),
                                        interval_src,
                                        fmt_opt_pct(hw.gpu_util_pct),
                                        fmt_opt_pct(hw.max_cpu_thread_pct)
                                    ));
                                }
                                let _ = self.ipc.set_target_fps(to_fps);
                                self.ceiling.on_step_down(to_fps);
                                game.current_cap = to_fps;
                                let enter_drop = is_cutscene;
                                game.in_drop_state = enter_drop;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: to_fps,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: enter_drop,
                                });
                            }
                            PacingAction::ScaleUp {
                                from_fps,
                                to_fps,
                                reason,
                            } => {
                                if now.duration_since(stable_since) >= Duration::from_secs(6) {
                                    self.ceiling.note_achieved_cap(from_fps);
                                }
                                log_info(format!(
                                    "[SCALE UP] ({}) {} -> {} FPS (gpu={}, cpu_thr={})",
                                    reason,
                                    from_fps,
                                    to_fps,
                                    fmt_opt_pct(hw.gpu_util_pct),
                                    fmt_opt_pct(hw.max_cpu_thread_pct)
                                ));
                                let _ = self.ipc.set_target_fps(to_fps);
                                let _ = self.ipc.set_limiter_enabled(true);
                                game.current_cap = to_fps;
                                game.limiter_enabled = true;
                                if let Ok(mut rec) = get_global_recorder().lock() {
                                    rec.clear_pid(pid);
                                }
                                let start_widx = self
                                    .ipc
                                    .read_frametime_ring(1)
                                    .map(|(idx, _)| idx)
                                    .unwrap_or(0);
                                self.probe_start_widx = start_widx;
                                next_state = Some(AutoState::ProbingUp {
                                    previous_cap: from_fps,
                                    probe_cap: to_fps,
                                    started_at: now,
                                    in_drop_state,
                                    reason,
                                });
                            }
                            PacingAction::MeasureNative => {
                                log_info(format!(
                                    "[BENCHMARK] {}s benchmark pulse -> disengaging limiter for {:.2}s to measure native FPS",
                                    if in_drop_state {
                                        self.config.drop_recheck_interval_secs
                                    } else {
                                        self.config.stable_recheck_interval_secs
                                    },
                                    self.config.measure_duration_secs
                                ));
                                let _ = self.ipc.set_limiter_enabled(false);
                                game.limiter_enabled = false;
                                if let Ok(mut rec) = get_global_recorder().lock() {
                                    rec.clear_pid(pid);
                                }
                                next_state = Some(AutoState::MeasuringNative {
                                    started_at: now,
                                    previous_cap: cap_fps,
                                });
                            }
                        }
                    }
                    AutoState::ProbingUp {
                        previous_cap,
                        probe_cap,
                        started_at,
                        in_drop_state,
                        reason,
                    } => {
                        let verify_ms = if previous_cap <= 35 {
                            self.config.probe_verify_ms.max(600)
                        } else {
                            self.config.probe_verify_ms
                        };
                        if now.duration_since(started_at) >= Duration::from_millis(verify_ms) {
                            if let Some(etw) = self.etw.as_ref() {
                                etw.flush();
                                self.last_etw_flush = now;
                            }

                            let pid = game.pid;
                            let (probe_intervals, probe_src) = self.get_probe_intervals(
                                pid,
                                Duration::from_millis(verify_ms),
                                now,
                            );
                            let hw = self.last_hw_load.clone();
                            let eval = self.analyzer.evaluate_probe_detailed(
                                previous_cap,
                                probe_cap,
                                &probe_intervals,
                                &hw,
                            );
                            let game = self.active_game.as_mut().unwrap();

                            if eval.passed {
                                self.ceiling.record_success(probe_cap);
                                game.current_cap = probe_cap;
                                game.in_drop_state = false;
                                let settle_ms = if hw
                                    .gpu_util_pct
                                    .is_some_and(|g| g < self.config.scale_up_gpu_util_pct)
                                {
                                    600
                                } else {
                                    1200
                                };
                                log_info(format!(
                                    "[PROBE PASS] ({}) locked {} FPS (measured={:.1}, samples={}, src={}, gpu={}, cpu_thr={})",
                                    reason,
                                    probe_cap,
                                    eval.sustained_fps,
                                    probe_intervals.len(),
                                    probe_src,
                                    fmt_opt_pct(hw.gpu_util_pct),
                                    fmt_opt_pct(hw.max_cpu_thread_pct)
                                ));
                                next_state = Some(AutoState::Settling {
                                    cap_fps: probe_cap,
                                    settle_until: now + Duration::from_millis(settle_ms),
                                    in_drop_state: false,
                                });
                            } else {
                                let _ = self.ipc.set_target_fps(previous_cap);
                                game.current_cap = previous_cap;
                                let step = self.config.step_fps.max(1);
                                let next_step = (previous_cap + step).min(self.config.max_fps);
                                if eval.is_software_lock {
                                    let is_temporary = in_drop_state
                                        || previous_cap < self.ceiling.session_high_cap;
                                    self.ceiling.record_software_lock(
                                        previous_cap,
                                        next_step,
                                        is_temporary,
                                        hw.gpu_util_pct,
                                        now,
                                    );
                                    log_info(format!(
                                        "[PROBE REVERT] ({}) {} -> {} FPS (measured={:.1}, samples={}, src={}, kind=soft_lock, temp={}, blocked={}, gpu={}, cpu_thr={}, backoff={:.0}s)",
                                        reason,
                                        probe_cap,
                                        previous_cap,
                                        eval.sustained_fps,
                                        probe_intervals.len(),
                                        probe_src,
                                        is_temporary,
                                        next_step,
                                        fmt_opt_pct(hw.gpu_util_pct),
                                        fmt_opt_pct(hw.max_cpu_thread_pct),
                                        self.ceiling.backoff_secs
                                    ));
                                } else {
                                    let hw_pegged = hw
                                        .gpu_util_pct
                                        .is_some_and(|g| g >= self.config.hold_gpu_util_pct)
                                        || hw
                                            .max_cpu_thread_pct
                                            .is_some_and(|c| c >= self.config.max_cpu_thread_util_pct);
                                    let block_cap = if hw_pegged
                                        && eval.sustained_fps >= (next_step as f64)
                                    {
                                        let floored =
                                            ((eval.sustained_fps / (step as f64)).floor() as u32)
                                                * step;
                                        (floored + step).clamp(next_step, probe_cap)
                                    } else {
                                        probe_cap
                                    };
                                    self.ceiling.record_failure(block_cap, hw.gpu_util_pct, now);
                                    log_info(format!(
                                        "[PROBE REVERT] ({}) {} -> {} FPS (measured={:.1}, samples={}, src={}, kind=hw_limit, blocked={}, gpu={}, cpu_thr={}, backoff={:.0}s)",
                                        reason,
                                        probe_cap,
                                        previous_cap,
                                        eval.sustained_fps,
                                        probe_intervals.len(),
                                        probe_src,
                                        block_cap,
                                        fmt_opt_pct(hw.gpu_util_pct),
                                        fmt_opt_pct(hw.max_cpu_thread_pct),
                                        self.ceiling.backoff_secs
                                    ));
                                }
                                next_state = Some(AutoState::Settling {
                                    cap_fps: previous_cap,
                                    settle_until: now + Duration::from_millis(1000),
                                    in_drop_state,
                                });
                            }
                        }
                    }
                    AutoState::MeasuringNative {
                        started_at,
                        previous_cap,
                    } => {
                        let warmup_secs = 0.25;
                        let total_measure_secs = self.config.measure_duration_secs + warmup_secs;
                        if now.duration_since(started_at)
                            >= Duration::from_secs_f64(total_measure_secs)
                        {
                            if let Some(etw) = self.etw.as_ref() {
                                etw.flush();
                                self.last_etw_flush = now;
                            }

                            let pid = game.pid;
                            let measured_fps = self.get_live_fps(
                                pid,
                                Duration::from_secs_f64(total_measure_secs),
                                Duration::from_secs_f64(warmup_secs),
                                true,
                                now,
                            );
                            let game = self.active_game.as_mut().unwrap();

                            if let Some(fps) = measured_fps {
                                let new_cap = self.config.clamp_and_round_fps(fps);
                                let stay_in_drop = game.in_drop_state && new_cap <= 35;
                                log_info(format!(
                                    "[BENCHMARK COMPLETE] Measured native: {:.2} FPS -> setting cap {} FPS (in_drop={})",
                                    fps, new_cap, stay_in_drop
                                ));

                                let _ = self.ipc.set_target_fps(new_cap);
                                let _ = self.ipc.set_limiter_enabled(true);
                                game.current_cap = new_cap;
                                game.limiter_enabled = true;
                                game.in_drop_state = stay_in_drop;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: new_cap,
                                    settle_until: now + Duration::from_millis(1000),
                                    in_drop_state: stay_in_drop,
                                });
                            } else if previous_cap > 0 {
                                log_warn(format!(
                                    "[BENCHMARK INCONCLUSIVE] No frames captured; restoring previous cap {} FPS",
                                    previous_cap
                                ));
                                let _ = self.ipc.set_target_fps(previous_cap);
                                let _ = self.ipc.set_limiter_enabled(true);
                                game.current_cap = previous_cap;
                                game.limiter_enabled = true;
                                next_state = Some(AutoState::Settling {
                                    cap_fps: previous_cap,
                                    settle_until: now + Duration::from_millis(1500),
                                    in_drop_state: game.in_drop_state,
                                });
                            } else {
                                next_state = Some(AutoState::Settling {
                                    cap_fps: 0,
                                    settle_until: now + Duration::from_millis(2000),
                                    in_drop_state: false,
                                });
                            }
                        }
                    }
                }
            }

            if let Some(ns) = next_state {
                if matches!(ns, AutoState::PacingActive { .. }) {
                    self.mark_ipc_window_start();
                }
                if let Some(game) = self.active_game.as_mut() {
                    game.state = ns;
                }
            }
        }

        // 5. Synchronize UI telemetry & periodic 60s diagnostic heartbeat
        let active_pid = self.active_game.as_ref().map(|g| g.pid);
        let measured_fps = active_pid.and_then(|pid| {
            self.get_live_fps(pid, Duration::from_secs(2), Duration::ZERO, false, now)
        });

        let is_measuring = self
            .active_game
            .as_ref()
            .map(|g| {
                matches!(
                    g.state,
                    AutoState::MeasuringNative { .. } | AutoState::ProbingUp { .. }
                )
            })
            .unwrap_or(false);

        let ipc_alive = self.ipc.is_alive();
        let (status_str, in_drop) = if !self.auto_enabled {
            ("PAUSED".to_string(), false)
        } else if !ipc_alive {
            ("NO FRAMEPACER".to_string(), false)
        } else if let Some(game) = &self.active_game {
            if game.is_backgrounded {
                ("GAME BACKGROUNDED".to_string(), game.in_drop_state)
            } else {
                match game.state {
                    AutoState::Idle => ("STANDBY".to_string(), false),
                    AutoState::MeasuringNative { .. } => {
                        ("BENCHMARKING NATIVE".to_string(), false)
                    }
                    AutoState::ProbingUp { .. } => {
                        ("PROBING UP".to_string(), game.in_drop_state)
                    }
                    AutoState::Settling {
                        in_drop_state: true,
                        ..
                    } => ("CUTSCENE LOCKING".to_string(), true),
                    AutoState::Settling { cap_fps: 0, .. } => {
                        ("AWAITING FRAMES".to_string(), false)
                    }
                    AutoState::Settling { .. } => ("SETTLING CADENCE".to_string(), false),
                    AutoState::PacingActive {
                        in_drop_state: true,
                        ..
                    } => ("CUTSCENE SNAP".to_string(), true),
                    AutoState::PacingActive { .. } => ("PACING LOCKED".to_string(), false),
                }
            }
        } else {
            ("WAITING FOR GAME".to_string(), false)
        };

        if let Some(game) = &self.active_game
            && now.duration_since(self.last_status_log) >= Duration::from_secs(60)
        {
            self.last_status_log = now;
            let fps_str = measured_fps
                .map(|f| format!("{:.1}", f))
                .unwrap_or_else(|| "n/a".to_string());
            let blocked_str = self
                .ceiling
                .blocked_cap
                .map(|b| format!("{}", b))
                .unwrap_or_else(|| "none".to_string());
            log_info(format!(
                "[STATUS] game={} cap={} fps={} gpu={} cpu_thr={} state={} blocked={} high={} backoff={:.0}s",
                game.process_name,
                game.current_cap,
                fps_str,
                fmt_opt_pct(self.last_hw_load.gpu_util_pct),
                fmt_opt_pct(self.last_hw_load.max_cpu_thread_pct),
                status_str,
                blocked_str,
                self.ceiling.session_high_cap,
                self.ceiling.backoff_secs
            ));
        }

        let display_cap = self
            .active_game
            .as_ref()
            .filter(|g| g.current_cap > 0)
            .map(|g| g.current_cap)
            .unwrap_or_else(|| {
                if ipc_alive {
                    self.ipc.get_target_fps().unwrap_or(0)
                } else {
                    0
                }
            });

        if let Ok(mut telem) = self.telemetry.lock() {
            telem.auto_enabled = self.auto_enabled;
            telem.active_game_name = self.active_game.as_ref().map(|g| g.process_name.clone());
            telem.active_game_pid = self.active_game.as_ref().map(|g| g.pid);
            telem.current_cap = display_cap;
            telem.measured_fps = measured_fps;
            telem.is_measuring_native = is_measuring;
            telem.pacing_status = status_str;
            telem.ipc_connected = ipc_alive;
            telem.in_drop_state = in_drop;
            telem.etw_active = self.etw.is_some();
            telem.is_elevated = self.is_elevated;
            telem.etw_error = self.etw_error.clone();
            telem.gpu_util_pct = self
                .active_game
                .as_ref()
                .and(self.last_hw_load.gpu_util_pct);
            telem.cpu_thread_pct = self
                .active_game
                .as_ref()
                .and(self.last_hw_load.max_cpu_thread_pct);
        }
    }

    fn shutdown(&mut self) {
        log_info("Shutting down FramePacer Companion...");
        if let Some(game) = self.active_game.take() {
            if game.initial_cap > 0 && game.current_cap != game.initial_cap {
                log_info(format!(
                    "Restoring initial cap {} FPS for {}",
                    game.initial_cap, game.process_name
                ));
                let _ = self.ipc.set_target_fps(game.initial_cap);
            }
            let _ = self.ipc.set_limiter_enabled(game.initial_limiter_enabled);
        }
        self.ipc.disconnect();
        if let Some(mut etw) = self.etw.take() {
            etw.stop();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir = exe_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let data_dir = config_dir.join("data");
    init_logger(data_dir.join("framepacer_companion.log"));

    let data_config = data_dir.join("config.toml");
    let config_path = if data_config.exists() {
        data_config
    } else if config_dir.join("config.toml").exists() {
        config_dir.join("config.toml")
    } else if data_dir.is_dir() {
        data_config
    } else {
        config_dir.join("config.toml")
    };

    log_info(format!(
        "=== FRAMEPACER COMPANION v0.3.0 === (elevated={}, config={})",
        is_process_elevated(),
        config_path.display()
    ));

    let telemetry = Arc::new(Mutex::new(UiTelemetry::default()));
    let (cmd_tx, cmd_rx) = channel::<UiCommand>();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();
    let engine_telemetry = telemetry.clone();

    // Spawn background pacing engine thread (running at 100ms cadence)
    let engine_thread = std::thread::spawn(move || {
        let mut engine = EngineController::new(config_path, engine_telemetry, cmd_rx);
        log_info("Pacing engine initialized.");

        while engine_running.load(Ordering::Relaxed) {
            engine.tick();
            std::thread::sleep(Duration::from_millis(100));
        }

        engine.shutdown();
        log_info("Pacing engine cleanly stopped.");
    });

    // Run Luxury Cyber GUI on the main thread (blocking until window closed)
    let gui_res = run_gui(telemetry, cmd_tx);

    // Stop background engine
    running.store(false, Ordering::Relaxed);
    let _ = engine_thread.join();

    if let Err(e) = gui_res {
        log_error(format!("GUI error: {}", e));
    }

    Ok(())
}
