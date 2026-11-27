use crate::config::AppConfig;
use crate::telemetry::HardwareLoad;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingAction {
    /// Keep current cap, pacing is smooth and flat.
    KeepCap,
    /// Stepping down by step_fps (e.g. 5) because of sustained late frames.
    StepDown { from_fps: u32, to_fps: u32 },
    /// Jumping directly to target_fps (e.g. 30 in cutscenes).
    JumpTo { from_fps: u32, to_fps: u32 },
    /// Disengage limiter briefly (for 1.0s) to measure native uncapped FPS (pulse mode).
    MeasureNative,
    /// Step or ramp upward while keeping the limiter engaged (never-off mode).
    ScaleUp {
        from_fps: u32,
        to_fps: u32,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoState {
    /// Waiting for game or disabled.
    Idle,
    /// Measuring native uncapped FPS for measure_duration_secs (limiter disengaged, pulse mode).
    MeasuringNative {
        started_at: Instant,
        previous_cap: u32,
    },
    /// Actively pacing at current cap (limiter engaged).
    PacingActive {
        cap_fps: u32,
        stable_since: Instant,
        in_drop_state: bool,
    },
    /// Testing a higher cap with the limiter still engaged (never-off mode).
    ProbingUp {
        previous_cap: u32,
        probe_cap: u32,
        started_at: Instant,
        in_drop_state: bool,
        reason: &'static str,
    },
    /// Cooldown period right after changing cap to allow frame pacing to settle.
    Settling {
        cap_fps: u32,
        settle_until: Instant,
        in_drop_state: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeEvaluation {
    pub passed: bool,
    pub sustained_fps: f64,
    pub is_software_lock: bool,
}

/// Tracks failed upward probes (e.g. in-game cap / VSync / engine limit) with adaptive backoff.
#[derive(Debug, Clone, Default)]
pub struct CeilingTracker {
    pub blocked_cap: Option<u32>,
    pub backoff_secs: f64,
    pub blocked_until: Option<Instant>,
    pub baseline_gpu_pct: Option<f64>,
    pub consecutive_up_passes: u32,
    pub session_high_cap: u32,
    pub soft_lock_cap: Option<u32>,
}

impl CeilingTracker {
    pub fn note_achieved_cap(&mut self, cap_fps: u32) {
        self.session_high_cap = self.session_high_cap.max(cap_fps);
    }

    pub fn is_soft_locked_at(&self, current_cap: u32) -> bool {
        self.soft_lock_cap == Some(current_cap)
    }

    pub fn record_software_lock(
        &mut self,
        locked_cap: u32,
        next_step_cap: u32,
        is_temporary: bool,
        gpu_pct: Option<f64>,
        now: Instant,
    ) {
        self.backoff_secs = if is_temporary {
            4.0
        } else if self.soft_lock_cap != Some(locked_cap) || self.backoff_secs < 10.0 {
            10.0
        } else {
            (self.backoff_secs + 5.0).min(20.0)
        };
        self.soft_lock_cap = Some(locked_cap);
        self.blocked_cap = Some(next_step_cap);
        self.blocked_until = Some(now + Duration::from_secs_f64(self.backoff_secs));
        self.baseline_gpu_pct = gpu_pct;
        self.consecutive_up_passes = 0;
    }

    pub fn record_failure(&mut self, failed_cap: u32, gpu_pct: Option<f64>, now: Instant) {
        self.soft_lock_cap = None;
        self.backoff_secs = if self.backoff_secs < 15.0 {
            15.0
        } else {
            (self.backoff_secs * 2.0).min(120.0)
        };
        self.blocked_cap = Some(failed_cap);
        self.blocked_until = Some(now + Duration::from_secs_f64(self.backoff_secs));
        self.baseline_gpu_pct = gpu_pct;
        self.consecutive_up_passes = 0;
    }

    pub fn record_success(&mut self, achieved_cap: u32) {
        self.consecutive_up_passes = self.consecutive_up_passes.saturating_add(1);
        if self.soft_lock_cap.is_some_and(|s| achieved_cap > s) {
            self.soft_lock_cap = None;
        }
        if self.blocked_cap.is_some_and(|b| achieved_cap >= b) {
            self.blocked_cap = None;
            self.blocked_until = None;
            self.backoff_secs = 0.0;
            self.baseline_gpu_pct = None;
        }
    }

    pub fn record_step_down_ceiling(
        &mut self,
        failed_cap: u32,
        new_cap: u32,
        gpu_pct: Option<f64>,
        now: Instant,
    ) {
        self.soft_lock_cap = None;
        self.consecutive_up_passes = 0;
        self.backoff_secs = if self.blocked_cap == Some(failed_cap) && self.backoff_secs >= 15.0 {
            (self.backoff_secs * 2.0).min(60.0)
        } else {
            15.0
        };
        self.blocked_cap = Some(failed_cap);
        self.blocked_until = Some(now + Duration::from_secs_f64(self.backoff_secs));
        self.baseline_gpu_pct = gpu_pct.map(|g| {
            if failed_cap > 0 {
                (g * (new_cap as f64 / failed_cap as f64)).min(g)
            } else {
                g
            }
        });
    }

    pub fn on_step_down(&mut self, new_cap: u32) {
        self.consecutive_up_passes = 0;
        if self.soft_lock_cap.is_some_and(|s| new_cap < s) {
            self.soft_lock_cap = None;
        }
        if self.blocked_cap.is_some_and(|b| new_cap + 5 < b) {
            self.blocked_cap = None;
            self.blocked_until = None;
            self.backoff_secs = 0.0;
            self.baseline_gpu_pct = None;
        }
    }

    pub fn maybe_clear_on_load_drop(&mut self, gpu_pct: Option<f64>) {
        if let (Some(base), Some(cur)) = (self.baseline_gpu_pct, gpu_pct)
            && (base - cur) >= 15.0
        {
            self.soft_lock_cap = None;
            self.blocked_cap = None;
            self.blocked_until = None;
            self.backoff_secs = 0.0;
            self.baseline_gpu_pct = None;
        }
    }

    pub fn is_blocked(&self, target_cap: u32, now: Instant) -> bool {
        self.blocked_cap.is_some_and(|b| target_cap >= b)
            && self.blocked_until.is_some_and(|u| now < u)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
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

    fn evaluate_downward_and_stats(
        &self,
        current_cap: u32,
        intervals: &[f64],
    ) -> Result<(f64, bool, bool), PacingAction> {
        if intervals.len() < 5 {
            return Err(PacingAction::KeepCap);
        }

        // 1. Instant Cutscene Drop Detection: inspect recent cadence tail
        let recent_count = intervals.len().min(15);
        let recent_slice = &intervals[intervals.len() - recent_count..];
        let recent_avg_ms = recent_slice.iter().sum::<f64>() / (recent_slice.len() as f64).max(1.0);
        let recent_fps = if recent_avg_ms > 0.0 {
            1000.0 / recent_avg_ms
        } else {
            current_cap as f64
        };
        let recent_late_frames = recent_slice.iter().filter(|&&dt| dt >= 28.0).count();

        if recent_fps <= 35.0
            && recent_late_frames >= (recent_count / 2)
            && current_cap >= (30 + self.config.step_fps)
            && (current_cap as f64 - recent_fps) >= (self.config.drop_detection_fps as f64)
        {
            return Err(PacingAction::JumpTo {
                from_fps: current_cap,
                to_fps: 30,
            });
        }

        // 2. Compute dynamic late threshold scaled to the step delta
        let nominal_ms = 1000.0 / (current_cap as f64);
        let step_down_fps = current_cap
            .saturating_sub(self.config.step_fps)
            .max(self.config.min_fps);
        let step_delta_ms = (1000.0 / (step_down_fps as f64)) - nominal_ms;
        let dynamic_tolerance = if step_delta_ms > 0.0 {
            (step_delta_ms * 0.45).clamp(0.4, self.config.spike_tolerance_ms)
        } else {
            self.config.spike_tolerance_ms.clamp(0.4, 1.0)
        };
        let late_threshold_ms = nominal_ms + dynamic_tolerance;

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
            let sum: f64 = trimmed.iter().sum();
            if sum <= 0.0 {
                nominal_ms
            } else {
                sum / (trimmed.len() as f64)
            }
        };
        let sustained_fps = if avg_ms > 0.0 {
            1000.0 / avg_ms
        } else {
            current_cap as f64
        };

        // 3. Evaluate downward pressure
        let has_sustained_shortfall = sustained_fps < (current_cap as f64 - 1.5);
        let has_sustained_late_frames = !is_isolated_spike
            && (late_ratio > self.config.late_frame_threshold_ratio || max_consecutive_late >= 3);

        if has_sustained_shortfall || has_sustained_late_frames {
            if (current_cap as f64 - sustained_fps) >= (self.config.drop_detection_fps as f64) {
                let mut target = self.config.clamp_and_round_fps(sustained_fps);
                if sustained_fps <= 35.0 {
                    target = 30;
                }
                if target < current_cap {
                    return Err(PacingAction::JumpTo {
                        from_fps: current_cap,
                        to_fps: target.max(self.config.min_fps),
                    });
                }
            }

            if current_cap > self.config.min_fps {
                let target = current_cap
                    .saturating_sub(self.config.step_fps)
                    .max(self.config.min_fps);
                if target < current_cap {
                    return Err(PacingAction::StepDown {
                        from_fps: current_cap,
                        to_fps: target,
                    });
                }
            }
        }

        let is_pacing_smooth = late_count == 0 || (is_isolated_spike && late_ratio < 0.03);
        let is_achieving_cap = sustained_fps >= (current_cap as f64 - 0.75);
        Ok((sustained_fps, is_pacing_smooth, is_achieving_cap))
    }

    /// Evaluates a window of frame intervals (in ms) against the current cap (Pulse mode).
    pub fn evaluate_intervals(
        &self,
        current_cap: u32,
        intervals: &[f64],
        now: Instant,
        stable_since: Instant,
        in_drop_state: bool,
    ) -> PacingAction {
        let (_sustained_fps, is_pacing_smooth, is_achieving_cap) =
            match self.evaluate_downward_and_stats(current_cap, intervals) {
                Ok(stats) => stats,
                Err(action) => return action,
            };

        if !in_drop_state && current_cap >= self.config.max_fps {
            return PacingAction::KeepCap;
        }

        let benchmark_interval = if in_drop_state {
            Duration::from_secs_f64(self.config.drop_recheck_interval_secs)
        } else {
            Duration::from_secs_f64(self.config.stable_recheck_interval_secs)
        };

        if is_pacing_smooth
            && is_achieving_cap
            && now.duration_since(stable_since) >= benchmark_interval
        {
            return PacingAction::MeasureNative;
        }

        PacingAction::KeepCap
    }

    /// Computes simple average FPS across `intervals` (ms) for diagnostic logging.
    pub fn summarize_intervals(intervals: &[f64]) -> Option<f64> {
        if intervals.is_empty() {
            return None;
        }
        let sum: f64 = intervals.iter().sum();
        if sum > 0.0 {
            Some((intervals.len() as f64) * 1000.0 / sum)
        } else {
            None
        }
    }

    /// Returns true if the current cap is smoothly achieved in Never-Off mode.
    pub fn is_cap_achieved(&self, current_cap: u32, intervals: &[f64]) -> bool {
        matches!(
            self.evaluate_downward_and_stats(current_cap, intervals),
            Ok((_, true, true))
        )
    }

    /// Evaluates whether a Never-Off upward probe (`probe_cap`) succeeded cleanly,
    /// and classifies failures into software/external locks vs hardware/engine limits.
    pub fn evaluate_probe_detailed(
        &self,
        previous_cap: u32,
        probe_cap: u32,
        intervals: &[f64],
        hw: &HardwareLoad,
    ) -> ProbeEvaluation {
        if intervals.len() < 5 {
            return ProbeEvaluation {
                passed: false,
                sustained_fps: 0.0,
                is_software_lock: false,
            };
        }

        let nominal_ms = 1000.0 / (probe_cap as f64);
        let step_down_fps = probe_cap
            .saturating_sub(self.config.step_fps)
            .max(self.config.min_fps);
        let step_delta_ms = (1000.0 / (step_down_fps as f64)) - nominal_ms;
        let dynamic_tolerance = if step_delta_ms > 0.0 {
            (step_delta_ms * 0.45).clamp(0.4, self.config.spike_tolerance_ms)
        } else {
            self.config.spike_tolerance_ms.clamp(0.4, 1.0)
        };
        let late_threshold_ms = nominal_ms + dynamic_tolerance;

        let mut late_count = 0usize;
        let mut consecutive_late = 0usize;
        let mut max_consecutive_late = 0usize;
        for &dt in intervals {
            if dt > late_threshold_ms {
                late_count += 1;
                consecutive_late += 1;
                max_consecutive_late = max_consecutive_late.max(consecutive_late);
            } else {
                consecutive_late = 0;
            }
        }
        let late_ratio = (late_count as f64) / (intervals.len() as f64);

        let mut sorted = intervals.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let trim = ((sorted.len() as f64) * 0.05).floor() as usize;
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
        let sustained_fps = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };

        let passed = sustained_fps >= (probe_cap as f64 - 1.5)
            && late_ratio <= 0.12
            && max_consecutive_late <= 3;

        if passed {
            return ProbeEvaluation {
                passed: true,
                sustained_fps,
                is_software_lock: false,
            };
        }

        let is_software_lock = if previous_cap > 0 {
            let prev_nominal_ms = 1000.0 / (previous_cap as f64);
            let prev_step_down = previous_cap
                .saturating_sub(self.config.step_fps)
                .max(self.config.min_fps);
            let prev_step_delta_ms = (1000.0 / (prev_step_down as f64)) - prev_nominal_ms;
            let prev_tol = if prev_step_delta_ms > 0.0 {
                (prev_step_delta_ms * 0.45).clamp(0.4, self.config.spike_tolerance_ms)
            } else {
                self.config.spike_tolerance_ms.clamp(0.4, 1.0)
            };
            let prev_late_threshold = prev_nominal_ms + prev_tol;
            let prev_late_ratio = (intervals
                .iter()
                .filter(|&&dt| dt > prev_late_threshold)
                .count() as f64)
                / (intervals.len() as f64);

            let pinned_at_prev = sustained_fps >= (previous_cap as f64 - 1.5)
                && sustained_fps <= (previous_cap as f64 + 1.0);
            let smooth_at_prev = prev_late_ratio <= 0.15;
            let hw_not_pegged = hw
                .gpu_util_pct
                .is_none_or(|g| g < self.config.hold_gpu_util_pct)
                && hw
                    .max_cpu_thread_pct
                    .is_none_or(|c| c < self.config.max_cpu_thread_util_pct);

            pinned_at_prev && smooth_at_prev && hw_not_pegged
        } else {
            false
        };

        ProbeEvaluation {
            passed: false,
            sustained_fps,
            is_software_lock,
        }
    }

    /// Evaluates whether a Never-Off upward probe (`probe_cap`) succeeded cleanly.
    pub fn evaluate_probe_result(&self, probe_cap: u32, intervals: &[f64]) -> bool {
        self.evaluate_probe_detailed(0, probe_cap, intervals, &HardwareLoad::default())
            .passed
    }

    /// Evaluates Never-Off headroom-guided pacing with zero limiter-off pulses.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_never_off(
        &self,
        current_cap: u32,
        intervals: &[f64],
        hw: &HardwareLoad,
        ceiling: &CeilingTracker,
        now: Instant,
        stable_since: Instant,
        in_drop_state: bool,
    ) -> PacingAction {
        let (_sustained_fps, is_pacing_smooth, is_achieving_cap) =
            match self.evaluate_downward_and_stats(current_cap, intervals) {
                Ok(stats) => stats,
                Err(action) => return action,
            };

        if current_cap >= self.config.max_fps || !is_pacing_smooth || !is_achieving_cap {
            return PacingAction::KeepCap;
        }

        let step = self.config.step_fps.max(1);
        let elapsed = now.duration_since(stable_since);

        // 1. Generalized invisible cutscene recovery probe for ANY drop tier (30, 48, 60, ...)
        let drop_gpu_limit = if current_cap <= 35 {
            92.0
        } else {
            self.config.hold_gpu_util_pct
        };
        let hw_has_headroom_for_drop = hw.gpu_util_pct.is_none_or(|g| g < drop_gpu_limit)
            && hw
                .max_cpu_thread_pct
                .is_none_or(|c| c < self.config.max_cpu_thread_util_pct);
        if in_drop_state && hw_has_headroom_for_drop {
            let probe_wait = if current_cap <= 35 {
                Duration::from_secs_f64(2.5)
            } else {
                Duration::from_secs_f64(3.0)
            };
            if elapsed >= probe_wait {
                let target = (current_cap + step).min(self.config.max_fps);
                if target > current_cap && !ceiling.is_blocked(target, now) {
                    return PacingAction::ScaleUp {
                        from_fps: current_cap,
                        to_fps: target,
                        reason: "cutscene_probe",
                    };
                }
            }
            return PacingAction::KeepCap;
        }

        // 2. Soft-lock invisible +step re-probe (never attempt large headroom jumps here)
        if ceiling.is_soft_locked_at(current_cap) {
            let target = (current_cap + step).min(self.config.max_fps);
            let hw_ok = hw
                .gpu_util_pct
                .is_none_or(|g| g < self.config.hold_gpu_util_pct)
                && hw
                    .max_cpu_thread_pct
                    .is_none_or(|c| c < self.config.max_cpu_thread_util_pct);
            if hw_ok
                && target > current_cap
                && elapsed >= Duration::from_secs(1)
                && !ceiling.is_blocked(target, now)
            {
                return PacingAction::ScaleUp {
                    from_fps: current_cap,
                    to_fps: target,
                    reason: "soft_lock_probe",
                };
            }
            return PacingAction::KeepCap;
        }

        // 3. Hardware headroom-guided scale-up when GPU PDH telemetry is available
        if let Some(gpu_pct) = hw.gpu_util_pct {
            let cpu_pct = hw.max_cpu_thread_pct.unwrap_or(0.0);
            let bottleneck = gpu_pct.max(cpu_pct);
            let required_stable = if ceiling.consecutive_up_passes > 0 {
                Duration::from_millis(800)
            } else {
                Duration::from_secs(2)
            };

            if gpu_pct < self.config.scale_up_gpu_util_pct
                && cpu_pct < self.config.max_cpu_thread_util_pct
                && elapsed >= required_stable
            {
                let est = (current_cap as f64)
                    * (self.config.target_gpu_util_pct / bottleneck.max(20.0));
                let raw_delta = (est - current_cap as f64) * 0.65;
                let step_f = step as f64;
                let delta = (((raw_delta / step_f).floor() * step_f) as u32).clamp(step, 30);
                let mut target = (current_cap + delta).min(self.config.max_fps);

                if ceiling.is_blocked(target, now) {
                    if let Some(blocked) = ceiling.blocked_cap {
                        let clamped = blocked.saturating_sub(step);
                        target = ((clamped / step) * step).min(self.config.max_fps);
                    }
                    if target <= current_cap || ceiling.is_blocked(target, now) {
                        return PacingAction::KeepCap;
                    }
                }

                if target > current_cap {
                    return PacingAction::ScaleUp {
                        from_fps: current_cap,
                        to_fps: target,
                        reason: "headroom",
                    };
                }
                return PacingAction::KeepCap;
            }

            // Hold zone (near target) or fast recovery / laptop downclocking wake-up nudge
            let nudge_target = (current_cap + step).min(self.config.max_fps);
            let is_near_limit = gpu_pct >= 93.0 || cpu_pct >= 90.0;
            let predicted_gpu = gpu_pct * ((nudge_target as f64) / (current_cap.max(1) as f64));
            let below_session_high = ceiling.session_high_cap > current_cap;
            let fast_recovery = below_session_high
                && gpu_pct < 84.0
                && predicted_gpu <= 90.0
                && cpu_pct < self.config.max_cpu_thread_util_pct;
            let nudge_wait = if fast_recovery {
                Duration::from_secs_f64(self.config.wakeup_nudge_interval_secs.min(8.0))
            } else {
                Duration::from_secs_f64(self.config.wakeup_nudge_interval_secs)
            };
            if !is_near_limit
                && elapsed >= nudge_wait
                && nudge_target > current_cap
                && !ceiling.is_blocked(nudge_target, now)
            {
                return PacingAction::ScaleUp {
                    from_fps: current_cap,
                    to_fps: nudge_target,
                    reason: if fast_recovery {
                        "recovery_nudge"
                    } else {
                        "wakeup_nudge"
                    },
                };
            }

            return PacingAction::KeepCap;
        }

        // 4. Fallback when GPU PDH counters are unavailable on a user's system
        if hw.max_cpu_thread_pct.unwrap_or(0.0) >= self.config.max_cpu_thread_util_pct {
            return PacingAction::KeepCap;
        }
        let wait = if ceiling.consecutive_up_passes > 0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(12)
        };
        let delta = match ceiling.consecutive_up_passes {
            0 => step,
            1 => step * 2,
            _ => step * 3,
        };
        let target = (current_cap + delta).min(self.config.max_fps);
        if elapsed >= wait && target > current_cap && !ceiling.is_blocked(target, now) {
            return PacingAction::ScaleUp {
                from_fps: current_cap,
                to_fps: target,
                reason: "blind_probe",
            };
        }

        PacingAction::KeepCap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cap_125_when_game_stuck_at_120_steps_down() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Target 125 FPS (8.0ms), but game hardware max is 120 FPS (8.33ms)
        let intervals = vec![8.333; 100];
        let action = analyzer.evaluate_intervals(125, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::StepDown {
                from_fps: 125,
                to_fps: 120
            }
        );
    }

    #[test]
    fn test_cap_120_when_game_drops_to_110_steps_down() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Target 120 FPS (8.33ms), game drops to 110 FPS (9.09ms)
        let intervals = vec![9.09; 100];
        let action = analyzer.evaluate_intervals(120, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::StepDown {
                from_fps: 120,
                to_fps: 115
            }
        );
    }

    #[test]
    fn test_cutscene_snap_is_instant_on_recent_cadence_drop() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // 80 frames at 16.66ms (60 FPS) followed by 20 frames at 33.33ms (30 FPS cutscene started 0.6s ago)
        let mut intervals = vec![16.666; 80];
        intervals.extend(vec![33.333; 20]);
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
    fn test_cap_65_when_game_stuck_at_60_steps_down() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        // Target 65 FPS (15.38ms), but game hardware max is 60 FPS (16.66ms)
        let intervals = vec![16.666; 100];
        let action = analyzer.evaluate_intervals(65, &intervals, now, now, false);
        assert_eq!(
            action,
            PacingAction::StepDown {
                from_fps: 65,
                to_fps: 60
            }
        );
    }

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
    fn test_benchmark_triggers_after_30_seconds_of_stable_frametimes() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(31); // >= 30s stable pacing
        let now = Instant::now();
        let intervals = vec![16.66; 100];
        let action = analyzer.evaluate_intervals(60, &intervals, now, past, false);
        assert_eq!(action, PacingAction::MeasureNative);
    }

    #[test]
    fn test_drop_state_triggers_benchmark_faster_after_4_seconds() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(5); // >= 4s drop recheck
        let now = Instant::now();
        // 30 FPS flat frames
        let intervals = vec![33.33; 100];
        let action = analyzer.evaluate_intervals(30, &intervals, now, past, true);
        assert_eq!(action, PacingAction::MeasureNative);
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
    fn test_flat_frametimes_before_30s_keeps_cap() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let past = Instant::now() - Duration::from_secs(10);
        let now = Instant::now();
        let intervals = vec![5.0; 100]; // 200 FPS
        let action = analyzer.evaluate_intervals(200, &intervals, now, past, false);
        // Under 30 seconds -> KeepCap
        assert_eq!(action, PacingAction::KeepCap);
    }

    #[test]
    fn test_high_fps_normal_jitter_stays_smooth() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let past = now - Duration::from_secs(31);
        // 120 FPS nominal = 8.333ms; normal ±0.34ms Windows/ETW jitter should stay smooth
        let mut intervals = Vec::with_capacity(100);
        for i in 0..100 {
            intervals.push(if i % 2 == 0 { 8.0 } else { 8.67 });
        }
        assert_eq!(
            analyzer.evaluate_intervals(120, &intervals, now, now, false),
            PacingAction::KeepCap
        );
        assert_eq!(
            analyzer.evaluate_intervals(120, &intervals, now, past, false),
            PacingAction::MeasureNative
        );
    }

    #[test]
    fn test_max_fps_skips_periodic_benchmark() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let past = now - Duration::from_secs(35);
        let intervals = vec![5.0; 100]; // Already at max_fps = 200
        assert_eq!(
            analyzer.evaluate_intervals(200, &intervals, now, past, false),
            PacingAction::KeepCap
        );
    }

    #[test]
    fn test_never_off_scales_up_on_low_gpu_and_holds_near_limit() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let stable = now - Duration::from_secs(3);
        let ceiling = CeilingTracker::default();
        let intervals = vec![16.66; 60]; // 60 FPS smooth

        let low_load = HardwareLoad {
            gpu_util_pct: Some(44.0),
            game_gpu_util_pct: Some(44.0),
            max_cpu_thread_pct: Some(25.0),
            pdh_available: true,
        };
        let action = analyzer.evaluate_never_off(60, &intervals, &low_load, &ceiling, now, stable, false);
        assert!(
            matches!(action, PacingAction::ScaleUp { from_fps: 60, to_fps, reason: "headroom" } if to_fps >= 85),
            "Expected large headroom ramp from 60 FPS at 44% GPU, got {:?}",
            action
        );

        let high_load = HardwareLoad {
            gpu_util_pct: Some(89.0),
            game_gpu_util_pct: Some(89.0),
            max_cpu_thread_pct: Some(40.0),
            pdh_available: true,
        };
        let intervals_90 = vec![11.11; 60]; // 90 FPS smooth
        assert_eq!(
            analyzer.evaluate_never_off(90, &intervals_90, &high_load, &ceiling, now, stable, false),
            PacingAction::KeepCap
        );
    }

    #[test]
    fn test_never_off_cutscene_probe_and_ceiling_backoff() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let stable = now - Duration::from_secs(3);
        let mut ceiling = CeilingTracker::default();
        let intervals_30 = vec![33.33; 40];
        let hw = HardwareLoad {
            gpu_util_pct: Some(45.0),
            game_gpu_util_pct: Some(45.0),
            max_cpu_thread_pct: Some(20.0),
            pdh_available: true,
        };

        assert_eq!(
            analyzer.evaluate_never_off(30, &intervals_30, &hw, &ceiling, now, stable, true),
            PacingAction::ScaleUp {
                from_fps: 30,
                to_fps: 35,
                reason: "cutscene_probe"
            }
        );

        // Simulate blocked ceiling at 65 FPS (e.g. in-game cap at 60)
        ceiling.record_failure(65, Some(50.0), now);
        let intervals_60 = vec![16.66; 60];
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &hw, &ceiling, now, stable, false),
            PacingAction::KeepCap
        );

        // If GPU load drops >= 15%, ceiling clears automatically
        ceiling.maybe_clear_on_load_drop(Some(30.0));
        assert!(!ceiling.is_blocked(65, now));
    }

    #[test]
    fn test_evaluate_probe_result() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let good = vec![15.38; 25]; // ~65 FPS
        assert!(analyzer.evaluate_probe_result(65, &good));

        let blocked = vec![16.66; 25]; // Stuck at 60 while probing 65
        assert!(!analyzer.evaluate_probe_result(65, &blocked));
    }

    #[test]
    fn test_200_to_60_cutscene_uses_plus5_probe_and_not_big_headroom_jump() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let stable = now - Duration::from_secs_f64(3.5);
        let ceiling = CeilingTracker::default();
        let intervals_60 = vec![16.66; 60];

        let cutscene_hw = HardwareLoad {
            gpu_util_pct: Some(30.0),
            game_gpu_util_pct: Some(30.0),
            max_cpu_thread_pct: Some(20.0),
            pdh_available: true,
        };
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &cutscene_hw, &ceiling, now, stable, true),
            PacingAction::ScaleUp {
                from_fps: 60,
                to_fps: 65,
                reason: "cutscene_probe"
            }
        );

        let boss_fight_hw = HardwareLoad {
            gpu_util_pct: Some(95.0),
            game_gpu_util_pct: Some(95.0),
            max_cpu_thread_pct: Some(40.0),
            pdh_available: true,
        };
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &boss_fight_hw, &ceiling, now, stable, true),
            PacingAction::KeepCap
        );
    }

    #[test]
    fn test_70_to_60_close_drop_software_lock_recovers_fast_without_120s_backoff() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let mut ceiling = CeilingTracker::default();
        ceiling.note_achieved_cap(70);

        let cutscene_hw = HardwareLoad {
            gpu_util_pct: Some(70.0),
            game_gpu_util_pct: Some(70.0),
            max_cpu_thread_pct: Some(30.0),
            pdh_available: true,
        };
        let pinned_60 = vec![16.66; 25];
        let eval = analyzer.evaluate_probe_detailed(60, 70, &pinned_60, &cutscene_hw);
        assert!(!eval.passed);
        assert!(eval.is_software_lock);

        ceiling.record_software_lock(60, 65, true, cutscene_hw.gpu_util_pct, now);
        assert_eq!(ceiling.backoff_secs, 4.0);
        assert!(ceiling.is_blocked(65, now + Duration::from_secs(2)));

        let intervals_60 = vec![16.66; 60];
        let at_2s = now + Duration::from_secs(2);
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &cutscene_hw, &ceiling, at_2s, now, false),
            PacingAction::KeepCap
        );

        let at_4_5s = now + Duration::from_secs_f64(4.5);
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &cutscene_hw, &ceiling, at_4_5s, now, false),
            PacingAction::ScaleUp {
                from_fps: 60,
                to_fps: 65,
                reason: "soft_lock_probe"
            }
        );

        // Repeated temporary soft-lock failures stay at 4.0s and never escalate to 120s
        ceiling.record_software_lock(60, 65, true, cutscene_hw.gpu_util_pct, at_4_5s);
        ceiling.record_software_lock(60, 65, true, cutscene_hw.gpu_util_pct, at_4_5s + Duration::from_secs(5));
        assert_eq!(ceiling.backoff_secs, 4.0);
    }

    #[test]
    fn test_70_to_60_close_drop_in_hold_zone_triggers_fast_recovery_nudge() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let stable = now - Duration::from_secs(9);
        let mut ceiling = CeilingTracker::default();
        ceiling.note_achieved_cap(70);

        let hold_zone_hw = HardwareLoad {
            gpu_util_pct: Some(82.0),
            game_gpu_util_pct: Some(82.0),
            max_cpu_thread_pct: Some(35.0),
            pdh_available: true,
        };
        let intervals_60 = vec![16.66; 60];
        assert_eq!(
            analyzer.evaluate_never_off(60, &intervals_60, &hold_zone_hw, &ceiling, now, stable, false),
            PacingAction::ScaleUp {
                from_fps: 60,
                to_fps: 65,
                reason: "recovery_nudge"
            }
        );
    }

    #[test]
    fn test_permanent_60_lock_blocks_65_directly_with_capped_20s_backoff() {
        let now = Instant::now();
        let mut ceiling = CeilingTracker::default();
        ceiling.note_achieved_cap(60);

        ceiling.record_software_lock(60, 65, false, Some(45.0), now);
        assert_eq!(ceiling.backoff_secs, 10.0);
        assert!(ceiling.is_blocked(65, now + Duration::from_secs(5)));

        ceiling.record_software_lock(60, 65, false, Some(45.0), now);
        assert_eq!(ceiling.backoff_secs, 15.0);

        ceiling.record_software_lock(60, 65, false, Some(45.0), now);
        assert_eq!(ceiling.backoff_secs, 20.0);

        ceiling.record_software_lock(60, 65, false, Some(45.0), now);
        assert_eq!(ceiling.backoff_secs, 20.0);
    }

    #[test]
    fn test_hardware_limited_and_ragged_probes_not_misclassified_as_software_lock() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());

        // (a) GPU pegged at 94% reaching ~61.7 FPS -> hardware-limited, not software lock
        let hw_pegged = HardwareLoad {
            gpu_util_pct: Some(94.0),
            game_gpu_util_pct: Some(94.0),
            max_cpu_thread_pct: Some(30.0),
            pdh_available: true,
        };
        let hw_fps_61_7 = vec![1000.0 / 61.7; 25];
        let eval_a = analyzer.evaluate_probe_detailed(60, 65, &hw_fps_61_7, &hw_pegged);
        assert!(!eval_a.passed);
        assert!(!eval_a.is_software_lock);

        // (b) Ragged fluctuating frames [15.5, 18.2, ...] even at 65% GPU -> not software lock
        let low_hw = HardwareLoad {
            gpu_util_pct: Some(65.0),
            game_gpu_util_pct: Some(65.0),
            max_cpu_thread_pct: Some(25.0),
            pdh_available: true,
        };
        let mut ragged = Vec::with_capacity(26);
        for i in 0..26 {
            ragged.push(if i % 2 == 0 { 15.5 } else { 18.2 });
        }
        let eval_b = analyzer.evaluate_probe_detailed(60, 65, &ragged, &low_hw);
        assert!(!eval_b.passed);
        assert!(!eval_b.is_software_lock);

        // (c) Too few samples (< 5) -> not software lock
        let eval_c = analyzer.evaluate_probe_detailed(60, 65, &[16.66; 4], &low_hw);
        assert!(!eval_c.passed);
        assert!(!eval_c.is_software_lock);
    }

    #[test]
    fn test_step_down_ceiling_prevents_immediate_ping_pong_and_clears_on_scene_lighten() {
        let now = Instant::now();
        let mut ceiling = CeilingTracker::default();

        // Step down 90 -> 85 at 97% GPU estimates baseline ~91.6% at 85 FPS and blocks 90 for 15s
        ceiling.record_step_down_ceiling(90, 85, Some(97.0), now);
        assert_eq!(ceiling.backoff_secs, 15.0);
        assert!(ceiling.is_blocked(90, now + Duration::from_secs(5)));

        // GPU dipping slightly to 85% is < 15% drop from 91.6% -> should NOT clear block yet
        ceiling.maybe_clear_on_load_drop(Some(85.0));
        assert!(ceiling.is_blocked(90, now + Duration::from_secs(6)));

        // Repeat failure at 90 escalates cooldown to 30s
        ceiling.record_step_down_ceiling(90, 85, Some(97.0), now);
        assert_eq!(ceiling.backoff_secs, 30.0);

        // Genuine scene lighten (gpu drops >= 15% below estimated 91.6% -> 74%) clears block
        ceiling.maybe_clear_on_load_drop(Some(74.0));
        assert!(!ceiling.is_blocked(90, now + Duration::from_secs(6)));
    }

    #[test]
    fn test_recovery_nudge_skips_when_predicted_gpu_exceeds_90() {
        let analyzer = PacingAnalyzer::new(AppConfig::default());
        let now = Instant::now();
        let stable = now - Duration::from_secs(12);
        let mut ceiling = CeilingTracker::default();
        ceiling.note_achieved_cap(185);

        let hw_87 = HardwareLoad {
            gpu_util_pct: Some(87.0),
            game_gpu_util_pct: Some(87.0),
            max_cpu_thread_pct: Some(50.0),
            pdh_available: true,
        };
        let intervals_85 = vec![1000.0 / 85.0; 60];
        assert_eq!(
            analyzer.evaluate_never_off(85, &intervals_85, &hw_87, &ceiling, now, stable, false),
            PacingAction::KeepCap
        );
    }
}
