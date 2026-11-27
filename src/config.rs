use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PacingMode {
    #[default]
    NeverOff,
    Pulse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub pacing_mode: PacingMode,

    #[serde(default = "default_target_gpu_util_pct")]
    pub target_gpu_util_pct: f64,

    #[serde(default = "default_hold_gpu_util_pct")]
    pub hold_gpu_util_pct: f64,

    #[serde(default = "default_scale_up_gpu_util_pct")]
    pub scale_up_gpu_util_pct: f64,

    #[serde(default = "default_max_cpu_thread_util_pct")]
    pub max_cpu_thread_util_pct: f64,

    #[serde(default = "default_probe_verify_ms")]
    pub probe_verify_ms: u64,

    #[serde(default = "default_wakeup_nudge_interval_secs")]
    pub wakeup_nudge_interval_secs: f64,

    #[serde(default = "default_min_fps")]
    pub min_fps: u32,

    #[serde(default = "default_max_fps")]
    pub max_fps: u32,

    #[serde(default = "default_step_fps")]
    pub step_fps: u32,

    #[serde(default = "default_measure_duration_secs")]
    pub measure_duration_secs: f64,

    #[serde(default = "default_stable_recheck_interval_secs")]
    pub stable_recheck_interval_secs: f64,

    #[serde(default = "default_drop_recheck_interval_secs")]
    pub drop_recheck_interval_secs: f64,

    #[serde(default = "default_spike_tolerance_ms")]
    pub spike_tolerance_ms: f64,

    #[serde(default = "default_late_frame_threshold_ratio")]
    pub late_frame_threshold_ratio: f64,

    #[serde(default = "default_drop_detection_fps")]
    pub drop_detection_fps: u32,

    #[serde(default = "default_hotkey_settle_ms")]
    pub hotkey_settle_ms: u64,

    #[serde(default = "default_framepacer_log_path")]
    pub framepacer_log_path: PathBuf,

    #[serde(default = "default_exclude_processes")]
    pub exclude_processes: Vec<String>,

    #[serde(default = "default_auto_enabled_at_startup")]
    pub auto_enabled_at_startup: bool,
}

fn default_target_gpu_util_pct() -> f64 {
    88.0
}
fn default_hold_gpu_util_pct() -> f64 {
    88.0
}
fn default_scale_up_gpu_util_pct() -> f64 {
    80.0
}
fn default_max_cpu_thread_util_pct() -> f64 {
    85.0
}
fn default_probe_verify_ms() -> u64 {
    500
}
fn default_wakeup_nudge_interval_secs() -> f64 {
    45.0
}
fn default_min_fps() -> u32 {
    30
}
fn default_max_fps() -> u32 {
    200
}
fn default_step_fps() -> u32 {
    5
}
fn default_measure_duration_secs() -> f64 {
    1.0
}
fn default_stable_recheck_interval_secs() -> f64 {
    30.0
}
fn default_drop_recheck_interval_secs() -> f64 {
    4.0
}
fn default_spike_tolerance_ms() -> f64 {
    2.0
}
fn default_late_frame_threshold_ratio() -> f64 {
    0.05
}
fn default_drop_detection_fps() -> u32 {
    15
}
fn default_hotkey_settle_ms() -> u64 {
    80
}
fn default_framepacer_log_path() -> PathBuf {
    PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\framepacer\framepacer.log")
}
fn default_exclude_processes() -> Vec<String> {
    Vec::new()
}
fn default_auto_enabled_at_startup() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            pacing_mode: PacingMode::default(),
            target_gpu_util_pct: default_target_gpu_util_pct(),
            hold_gpu_util_pct: default_hold_gpu_util_pct(),
            scale_up_gpu_util_pct: default_scale_up_gpu_util_pct(),
            max_cpu_thread_util_pct: default_max_cpu_thread_util_pct(),
            probe_verify_ms: default_probe_verify_ms(),
            wakeup_nudge_interval_secs: default_wakeup_nudge_interval_secs(),
            min_fps: default_min_fps(),
            max_fps: default_max_fps(),
            step_fps: default_step_fps(),
            measure_duration_secs: default_measure_duration_secs(),
            stable_recheck_interval_secs: default_stable_recheck_interval_secs(),
            drop_recheck_interval_secs: default_drop_recheck_interval_secs(),
            spike_tolerance_ms: default_spike_tolerance_ms(),
            late_frame_threshold_ratio: default_late_frame_threshold_ratio(),
            drop_detection_fps: default_drop_detection_fps(),
            hotkey_settle_ms: default_hotkey_settle_ms(),
            framepacer_log_path: default_framepacer_log_path(),
            exclude_processes: default_exclude_processes(),
            auto_enabled_at_startup: default_auto_enabled_at_startup(),
        }
    }
}

impl AppConfig {
    pub fn load_or_default(path: &Path) -> Self {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(cfg) = toml::from_str::<AppConfig>(&content)
        {
            return cfg;
        }
        let cfg = AppConfig::default();
        if let Ok(toml_str) = toml::to_string_pretty(&cfg) {
            let _ = std::fs::write(path, toml_str);
        }
        cfg
    }

    /// Snaps an FPS value to the nearest step within [min_fps, max_fps].
    /// Rounds down to ensure sustainable frame pacing headroom (per spec: "cap = native rounded down to 5"),
    /// with a small 0.2 FPS tolerance to accommodate VSync / timer jitter (e.g. 59.8 -> 60, but 64.6 -> 60).
    pub fn clamp_and_round_fps(&self, fps: f64) -> u32 {
        let step = self.step_fps.max(1) as f64;
        let rounded = ((fps + 0.2) / step).floor() * step;
        let rounded_u32 = rounded as u32;
        rounded_u32.clamp(self.min_fps, self.max_fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.pacing_mode, PacingMode::NeverOff);
        assert_eq!(cfg.min_fps, 30);
        assert_eq!(cfg.max_fps, 200);
        assert_eq!(cfg.step_fps, 5);
        assert!(cfg.exclude_processes.is_empty());
        assert!(cfg.auto_enabled_at_startup);
    }

    #[test]
    fn test_clamp_and_round() {
        let cfg = AppConfig::default();
        // 59.8 is near 60 (within 0.2 FPS tolerance) -> 60
        assert_eq!(cfg.clamp_and_round_fps(59.8), 60);
        // 58.3 cannot hold 60 FPS -> rounded down to 55 per pacing headroom spec
        assert_eq!(cfg.clamp_and_round_fps(58.3), 55);
        // 64.6 cannot hold 65 FPS -> rounded down to 60
        assert_eq!(cfg.clamp_and_round_fps(64.6), 60);
        // 52.1 cannot hold 55 FPS -> rounded down to 50
        assert_eq!(cfg.clamp_and_round_fps(52.1), 50);
        // Under min_fps (30) -> clamps to 30
        assert_eq!(cfg.clamp_and_round_fps(22.0), 30);
        // Over max_fps (200) -> clamps to 200
        assert_eq!(cfg.clamp_and_round_fps(250.0), 200);
        // 29.8 in a 30 FPS cutscene -> 30
        assert_eq!(cfg.clamp_and_round_fps(29.8), 30);
    }
}
