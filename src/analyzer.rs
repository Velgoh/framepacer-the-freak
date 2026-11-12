use crate::config::AppConfig;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingAction {
    /// Keep current cap, pacing is smooth and flat.
    KeepCap,
    /// Stepping down by step_fps (e.g. 5) because of sustained late frames.
    StepDown { from_fps: u32, to_fps: u32 },
    /// Jumping directly to target_fps (e.g. 30 in cutscenes).
    JumpTo { from_fps: u32, to_fps: u32 },
    /// Gently probing +5 FPS when frametimes have been flat for >= 8s.
    ProbeUp { from_fps: u32, to_fps: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoState {
    /// Waiting for game or disabled.
    Idle,
    /// Actively pacing at current cap (limiter NEVER turns off).
    PacingActive {
        cap_fps: u32,
        stable_since: Instant,
        in_drop_state: bool,
    },
    /// Cooldown period right after changing cap to allow frame pacing to settle.
    Settling {
        cap_fps: u32,
        settle_until: Instant,
        in_drop_state: bool,
    },
}

#[derive(Debug)]
pub struct PacingAnalyzer {
    pub config: AppConfig,
    pub state: AutoState,
    pub initial_game_cap: Option<u32>,
}

impl PacingAnalyzer {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            state: AutoState::Idle,
            initial_game_cap: None,
        }
    }

    /// Evaluates a window of frame intervals (in ms) against the current cap.
    /// In Never-Off pacing, the limiter is NEVER turned off:
    /// - Late frames / cutscenes trigger StepDown or JumpTo(30).
    /// - Stable flat frametimes (>= 8s) gently probe +5 FPS upward.
    pub fn evaluate_intervals(
        &self,
        current_cap: u32,
        intervals: &[f64],
        now: Instant,
        stable_since: Instant,
        in_drop_state: bool,
    ) -> PacingAction {
        if intervals.len() < 5 {
            return PacingAction::KeepCap;
        }

        // 1. Analyze pacing flatness against nominal frame time
        let nominal_ms = 1000.0 / (current_cap as f64);
        let late_threshold_ms = nominal_ms + self.config.spike_tolerance_ms;

        let mut late_count = 0;
        let mut consecutive_late = 0;
        let mut max_consecutive_late = 0;

        for &dt in intervals {
            if dt > late_threshold_ms {
                late_count += 1;
                consecutive_late += 1;
                if consecutive_late > max_consecutive_late {
                    max_consecutive_late = consecutive_late;
                }
            } else {
                consecutive_late = 0;
            }
        }

        let total_frames = intervals.len() as f64;
        let late_ratio = (late_count as f64) / total_frames;

        let is_isolated_spike = max_consecutive_late <= 2 && late_ratio < 0.08;

        // 2. If sustained late frames occur, evaluate downward pressure
        if !is_isolated_spike
            && (late_ratio > self.config.late_frame_threshold_ratio || max_consecutive_late >= 3)
        {
            // Compute robust sustained FPS (trimming top/bottom 5% outliers)
            let mut sorted = intervals.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let trim = (sorted.len() as f64 * 0.05).floor() as usize;
            let trimmed = if trim * 2 < sorted.len() {
                &sorted[trim..sorted.len() - trim]
            } else {
                &sorted[..]
            };
            let avg_ms = if trimmed.is_empty() {
                nominal_ms
            } else {
                trimmed.iter().sum::<f64>() / (trimmed.len() as f64)
            };
            let sustained_fps = 1000.0 / avg_ms;

            // Cutscene / massive drop check (e.g. 60 -> 30, drop >= drop_detection_fps)
            if (current_cap as f64 - sustained_fps) >= (self.config.drop_detection_fps as f64) {
                let mut target = self.config.clamp_and_round_fps(sustained_fps);
                // If sustained FPS is near 30 FPS cutscene, snap cleanly to 30
                if sustained_fps <= 35.0 {
                    target = 30;
                }
                if target < current_cap {
                    return PacingAction::JumpTo {
                        from_fps: current_cap,
                        to_fps: target.max(self.config.min_fps),
                    };
                }
            }

            // Normal step down by step_fps
            if current_cap > self.config.min_fps {
                let target = current_cap
                    .saturating_sub(self.config.step_fps)
                    .max(self.config.min_fps);
                if target < current_cap {
                    return PacingAction::StepDown {
                        from_fps: current_cap,
                        to_fps: target,
                    };
                }
            }
        }

        // 3. Frametimes are flat and pacing is smooth: check for upward probe
        let probe_interval = if in_drop_state {
            Duration::from_secs_f64(self.config.drop_recheck_interval_secs)
        } else {
            Duration::from_secs_f64(self.config.stable_recheck_interval_secs)
        };

        if now.duration_since(stable_since) >= probe_interval && current_cap < self.config.max_fps {
            let next_cap = (current_cap + self.config.step_fps).min(self.config.max_fps);
            return PacingAction::ProbeUp {
                from_fps: current_cap,
                to_fps: next_cap,
            };
        }

        PacingAction::KeepCap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_perfect_pacing_keeps_cap_before_probe_interval() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // 60 FPS -> 16.666 ms
        let intervals = vec![16.66; 100];
        // stable_since is now (0s elapsed < 8s probe interval)
        let action = analyzer.evaluate_intervals(60, &intervals, now, now, false);
        assert_eq!(action, PacingAction::KeepCap);
    }

    #[test]
    fn test_isolated_loading_spike_is_ignored() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // 100 frames at 16.6ms, with one single 500ms spike at frame 50
        let mut intervals = vec![16.66; 100];
        intervals[50] = 500.0;
        let action = analyzer.evaluate_intervals(60, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::KeepCap,
            "Single isolated spike must be ignored"
        );
    }

    #[test]
    fn test_sustained_heavy_area_triggers_step_down() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Target 60 FPS (16.6ms), but game rendering 52 FPS (19.2ms) repeatedly
        let intervals = vec![19.23; 100];
        let action = analyzer.evaluate_intervals(60, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::StepDown {
                from_fps: 60,
                to_fps: 55
            }
        );
    }

    #[test]
    fn test_cutscene_drop_triggers_instant_snap_to_30() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Target 60 FPS, game drops to 30 FPS cutscene (33.3ms)
        let intervals = vec![33.33; 100];
        let action = analyzer.evaluate_intervals(60, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::JumpTo {
                from_fps: 60,
                to_fps: 30
            }
        );
    }

    #[test]
    fn test_never_off_probes_plus_5_when_flat_for_8_seconds() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(9); // >= 8s flat line
        let now = Instant::now();
        let intervals = vec![16.66; 100];
        let action = analyzer.evaluate_intervals(60, &intervals, now, past, false);
        assert_eq!(
            action,
            PacingAction::ProbeUp {
                from_fps: 60,
                to_fps: 65
            }
        );
    }

    #[test]
    fn test_drop_state_probes_up_faster_to_resume_gameplay() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(5); // >= 4s drop probe
        let now = Instant::now();
        // 30 FPS flat frames
        let intervals = vec![33.33; 100];
        let action = analyzer.evaluate_intervals(30, &intervals, now, past, true);
        assert_eq!(
            action,
            PacingAction::ProbeUp {
                from_fps: 30,
                to_fps: 35
            }
        );
    }

    #[test]
    fn test_respects_minimum_fps() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Already at min_fps (30), game running at 20 FPS
        let intervals = vec![50.0; 100];
        let action = analyzer.evaluate_intervals(30, &intervals, now, now, false);
        // Cannot step down below 30
        assert_eq!(action, PacingAction::KeepCap);
    }

    #[test]
    fn test_empty_and_single_intervals() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        assert_eq!(
            analyzer.evaluate_intervals(60, &[], now, now, false),
            PacingAction::KeepCap
        );
        assert_eq!(
            analyzer.evaluate_intervals(60, &[16.6], now, now, false),
            PacingAction::KeepCap
        );
        // Under 5 frames: always KeepCap
        assert_eq!(
            analyzer.evaluate_intervals(60, &[20.0, 20.0, 20.0, 20.0], now, now, false),
            PacingAction::KeepCap
        );
    }

    #[test]
    fn test_cutscene_scenario_200_to_30() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Native was 200 FPS (5.0ms), cutscene drops to 30 FPS (33.33ms)
        let intervals = vec![33.33; 100];
        let action = analyzer.evaluate_intervals(200, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::JumpTo {
                from_fps: 200,
                to_fps: 30
            }
        );
    }

    #[test]
    fn test_probe_caps_at_max_fps() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(10);
        let now = Instant::now();
        let intervals = vec![5.0; 100]; // 200 FPS
        let action = analyzer.evaluate_intervals(200, &intervals, now, past, false);
        // At max_fps (200), cannot probe higher -> KeepCap
        assert_eq!(action, PacingAction::KeepCap);
    }
}
