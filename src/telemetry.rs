use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::Win32::System::Threading::{
    GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows::core::{PCWSTR, PWSTR, w};

const PDH_MORE_DATA: u32 = 0x8000_07D2;

/// Smoothed hardware utilization snapshot for the active game process.
#[derive(Debug, Clone, Default)]
pub struct HardwareLoad {
    /// Peak render-engine utilization (0..=100%) on the game's active GPU LUID.
    pub gpu_util_pct: Option<f64>,
    /// The game PID's own peak render-engine utilization (0..=100%).
    pub game_gpu_util_pct: Option<f64>,
    /// Utilization (0..=100%) of the single busiest thread in the game process.
    pub max_cpu_thread_pct: Option<f64>,
    /// Whether Windows PDH GPU Engine counters initialized cleanly.
    pub pdh_available: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuEngineSample {
    pub pid: u32,
    pub luid: String,
    pub engine_key: String,
    pub is_render_engine: bool,
    pub util_pct: f64,
}

/// Parses a Windows PDH `\GPU Engine(...)` instance name such as
/// `pid_9828_luid_0x00000000_0x0000e9bc_phys_0_eng_9_engtype_graphics_1`.
pub fn parse_gpu_engine_instance(name: &str, util_pct: f64) -> Option<GpuEngineSample> {
    if !util_pct.is_finite() || util_pct < 0.0 {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let pid_rest = lower.strip_prefix("pid_")?;
    let luid_marker = pid_rest.find("_luid_")?;
    let pid: u32 = pid_rest[..luid_marker].parse().ok()?;

    let after_luid = &pid_rest[luid_marker + 6..];
    let phys_marker = after_luid.find("_phys_")?;
    let luid = after_luid[..phys_marker].to_string();
    let engine_key = after_luid[phys_marker + 1..].to_string();

    let is_render_engine = if let Some(engtype_pos) = engine_key.find("engtype_") {
        let engtype = &engine_key[engtype_pos + 8..];
        engtype.starts_with("3d")
            || engtype.starts_with("graphics")
            || engtype.starts_with("compute")
    } else {
        false
    };

    Some(GpuEngineSample {
        pid,
        luid,
        engine_key,
        is_render_engine,
        util_pct,
    })
}

/// Aggregates PDH GPU engine samples into `(luid_peak_render_pct, game_peak_render_pct)`.
/// Sums all PIDs sharing the same `(luid, engine_key)` so background desktop/3D load on the
/// game's active GPU is accounted for without double-counting distinct engines.
pub fn aggregate_gpu_utilization(
    samples: &[GpuEngineSample],
    target_pid: u32,
) -> (Option<f64>, Option<f64>) {
    let mut best_luid: Option<(&str, f64)> = None;
    for s in samples {
        if !s.is_render_engine || s.pid != target_pid {
            continue;
        }
        match best_luid {
            Some((_, cur_max)) if s.util_pct <= cur_max => {}
            _ => best_luid = Some((s.luid.as_str(), s.util_pct)),
        }
    }

    let Some((target_luid, game_max)) = best_luid else {
        return (None, None);
    };

    let mut per_engine_sum: HashMap<&str, f64> = HashMap::new();
    for s in samples {
        if s.is_render_engine && s.luid == target_luid {
            *per_engine_sum.entry(s.engine_key.as_str()).or_insert(0.0) += s.util_pct;
        }
    }

    let luid_max = per_engine_sum
        .values()
        .copied()
        .fold(0.0_f64, f64::max)
        .clamp(0.0, 100.0);

    (Some(luid_max), Some(game_max.clamp(0.0, 100.0)))
}

unsafe fn pwstr_to_string(ptr: PWSTR) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while len < 512 && *ptr.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, len))
    }
}

/// Vendor-agnostic GPU utilization monitor backed by Windows PDH `\GPU Engine(*)\Utilization Percentage`.
pub struct GpuPdhMonitor {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

unsafe impl Send for GpuPdhMonitor {}

impl GpuPdhMonitor {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 || query.is_invalid() {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(
                query,
                w!(r"\GPU Engine(*)\Utilization Percentage"),
                0,
                &mut counter,
            ) != 0
                || counter.is_invalid()
            {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // Prime baseline sample for rate counters
            let _ = PdhCollectQueryData(query);
            Some(Self { query, counter })
        }
    }

    pub fn sample(&mut self, target_pid: u32) -> (Option<f64>, Option<f64>) {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return (None, None);
            }

            let mut buf_size = 0u32;
            let mut item_count = 0u32;
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut item_count,
                None,
            );
            if status != PDH_MORE_DATA || buf_size == 0 || item_count == 0 {
                return (None, None);
            }

            let mut backing = vec![0u64; (buf_size as usize).div_ceil(8)];
            let ptr = backing.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let status2 = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buf_size,
                &mut item_count,
                Some(ptr),
            );
            if status2 != 0 || item_count == 0 {
                return (None, None);
            }

            let items = std::slice::from_raw_parts(ptr, item_count as usize);
            let mut samples = Vec::new();
            for item in items {
                if item.FmtValue.CStatus != 0 {
                    continue;
                }
                let name = pwstr_to_string(item.szName);
                let val = item.FmtValue.Anonymous.doubleValue;
                if let Some(parsed) = parse_gpu_engine_instance(&name, val) {
                    samples.push(parsed);
                }
            }
            aggregate_gpu_utilization(&samples, target_pid)
        }
    }
}

impl Drop for GpuPdhMonitor {
    fn drop(&mut self) {
        unsafe {
            if !self.query.is_invalid() {
                let _ = PdhCloseQuery(self.query);
            }
        }
    }
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// Measures single-thread CPU saturation inside the target game process via `GetThreadTimes`.
pub struct ProcessCpuMonitor {
    last_pid: u32,
    last_sample_at: Option<Instant>,
    thread_times: HashMap<u32, u64>,
}

impl Default for ProcessCpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessCpuMonitor {
    pub fn new() -> Self {
        Self {
            last_pid: 0,
            last_sample_at: None,
            thread_times: HashMap::new(),
        }
    }

    pub fn reset(&mut self) {
        self.last_pid = 0;
        self.last_sample_at = None;
        self.thread_times.clear();
    }

    pub fn sample(&mut self, target_pid: u32, now: Instant) -> Option<f64> {
        if target_pid == 0 {
            return None;
        }
        if target_pid != self.last_pid {
            self.reset();
            self.last_pid = target_pid;
        }

        let current_map = unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                return None;
            };
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut map = HashMap::new();
            if Thread32First(snap, &mut entry).is_ok() {
                loop {
                    if entry.th32OwnerProcessID == target_pid
                        && let Ok(th) =
                            OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
                    {
                        let mut c = FILETIME::default();
                        let mut e = FILETIME::default();
                        let mut k = FILETIME::default();
                        let mut u = FILETIME::default();
                        if GetThreadTimes(th, &mut c, &mut e, &mut k, &mut u).is_ok() {
                            map.insert(
                                entry.th32ThreadID,
                                filetime_to_u64(k).saturating_add(filetime_to_u64(u)),
                            );
                        }
                        let _ = CloseHandle(th);
                    }
                    if Thread32Next(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            map
        };

        let mut max_thread_pct: Option<f64> = None;
        if let Some(prev_at) = self.last_sample_at {
            let dt_secs = now.duration_since(prev_at).as_secs_f64();
            if dt_secs >= 0.05 {
                let dt_100ns = dt_secs * 10_000_000.0;
                let mut peak = 0.0_f64;
                let mut matched = false;
                for (&tid, &cur_ticks) in &current_map {
                    if let Some(&prev_ticks) = self.thread_times.get(&tid) {
                        let delta = cur_ticks.saturating_sub(prev_ticks);
                        let pct = ((delta as f64) / dt_100ns) * 100.0;
                        if pct > peak {
                            peak = pct;
                        }
                        matched = true;
                    }
                }
                if matched {
                    max_thread_pct = Some(peak.clamp(0.0, 100.0));
                }
            }
        }

        self.last_sample_at = Some(now);
        self.thread_times = current_map;
        max_thread_pct
    }
}

fn ema_smooth(prev: Option<f64>, next: Option<f64>) -> Option<f64> {
    match (prev, next) {
        (Some(p), Some(n)) => Some((0.55 * n + 0.45 * p).clamp(0.0, 100.0)),
        (None, Some(n)) => Some(n.clamp(0.0, 100.0)),
        (Some(p), None) => Some(p),
        (None, None) => None,
    }
}

fn apply_gpu_sample_with_grace(
    prev_gpu: Option<f64>,
    prev_game_gpu: Option<f64>,
    raw_gpu: Option<f64>,
    raw_game_gpu: Option<f64>,
    misses: &mut u32,
) -> (Option<f64>, Option<f64>) {
    if raw_gpu.is_some() {
        *misses = 0;
        (
            ema_smooth(prev_gpu, raw_gpu),
            ema_smooth(prev_game_gpu, raw_game_gpu),
        )
    } else {
        *misses = misses.saturating_add(1);
        if *misses <= 2 {
            (prev_gpu, prev_game_gpu)
        } else {
            (None, None)
        }
    }
}

/// Polls GPU + per-thread CPU utilization at a throttled 400ms cadence with EMA smoothing.
pub struct HardwareMonitor {
    gpu_pdh: Option<GpuPdhMonitor>,
    cpu: ProcessCpuMonitor,
    last_poll: Option<Instant>,
    consecutive_gpu_misses: u32,
    cached: HardwareLoad,
}

impl Default for HardwareMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareMonitor {
    pub fn new() -> Self {
        let gpu_pdh = GpuPdhMonitor::new();
        let pdh_available = gpu_pdh.is_some();
        Self {
            gpu_pdh,
            cpu: ProcessCpuMonitor::new(),
            last_poll: None,
            consecutive_gpu_misses: 0,
            cached: HardwareLoad {
                pdh_available,
                ..Default::default()
            },
        }
    }

    pub fn pdh_available(&self) -> bool {
        self.gpu_pdh.is_some()
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
        self.last_poll = None;
        self.consecutive_gpu_misses = 0;
        self.cached = HardwareLoad {
            pdh_available: self.gpu_pdh.is_some(),
            ..Default::default()
        };
    }

    pub fn sample(&mut self, pid: u32, now: Instant) -> HardwareLoad {
        if let Some(prev) = self.last_poll
            && now.duration_since(prev) < Duration::from_millis(400)
        {
            return self.cached.clone();
        }
        self.last_poll = Some(now);

        let (raw_gpu, raw_game_gpu) = match self.gpu_pdh.as_mut() {
            Some(pdh) => pdh.sample(pid),
            None => (None, None),
        };
        let raw_cpu = self.cpu.sample(pid, now);

        let (gpu_util_pct, game_gpu_util_pct) = apply_gpu_sample_with_grace(
            self.cached.gpu_util_pct,
            self.cached.game_gpu_util_pct,
            raw_gpu,
            raw_game_gpu,
            &mut self.consecutive_gpu_misses,
        );
        if raw_gpu.is_none()
            && (self.consecutive_gpu_misses == 6
                || (self.consecutive_gpu_misses > 6
                    && self.consecutive_gpu_misses.is_multiple_of(25)))
            && let Some(fresh) = GpuPdhMonitor::new()
        {
            self.gpu_pdh = Some(fresh);
        }
        let max_cpu_thread_pct = ema_smooth(self.cached.max_cpu_thread_pct, raw_cpu);

        self.cached = HardwareLoad {
            gpu_util_pct,
            game_gpu_util_pct,
            max_cpu_thread_pct,
            pdh_available: self.gpu_pdh.is_some(),
        };
        self.cached.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gpu_engine_instance_dx12_and_3d() {
        let s1 = parse_gpu_engine_instance(
            "pid_9828_luid_0x00000000_0x0000e9bc_phys_0_eng_9_engtype_graphics_1",
            93.4,
        )
        .expect("Should parse DX12 graphics_1 engine");
        assert_eq!(s1.pid, 9828);
        assert_eq!(s1.luid, "0x00000000_0x0000e9bc");
        assert_eq!(s1.engine_key, "phys_0_eng_9_engtype_graphics_1");
        assert!(s1.is_render_engine);
        assert!((s1.util_pct - 93.4).abs() < 0.001);

        let s_copy = parse_gpu_engine_instance(
            "pid_9828_luid_0x00000000_0x0000e9bc_phys_0_eng_5_engtype_copy",
            12.0,
        )
        .expect("Should parse copy engine");
        assert!(!s_copy.is_render_engine);
    }

    #[test]
    fn test_aggregate_gpu_utilization_matches_mhw_luid_layout() {
        let samples = vec![
            parse_gpu_engine_instance(
                "pid_9828_luid_0x00000000_0x0000e9bc_phys_0_eng_9_engtype_graphics_1",
                93.4,
            )
            .unwrap(),
            parse_gpu_engine_instance(
                "pid_9828_luid_0x00000000_0x0000e9bc_phys_0_eng_11_engtype_compute_1",
                5.6,
            )
            .unwrap(),
            parse_gpu_engine_instance(
                "pid_10048_luid_0x00000000_0x0000e9bc_phys_0_eng_0_engtype_3d",
                25.6,
            )
            .unwrap(),
            parse_gpu_engine_instance(
                "pid_1488_luid_0x00000000_0x0000e9bc_phys_0_eng_0_engtype_3d",
                24.0,
            )
            .unwrap(),
            // Integrated GPU on another LUID should not affect target LUID
            parse_gpu_engine_instance(
                "pid_2222_luid_0x00000000_0x00001111_phys_0_eng_0_engtype_3d",
                80.0,
            )
            .unwrap(),
        ];

        let (luid_max, game_max) = aggregate_gpu_utilization(&samples, 9828);
        assert!((luid_max.unwrap() - 93.4).abs() < 0.01);
        assert!((game_max.unwrap() - 93.4).abs() < 0.01);

        // Shared 3D engine sums across PIDs on same LUID
        let shared = vec![
            parse_gpu_engine_instance(
                "pid_5000_luid_0x00000000_0x0000aaaa_phys_0_eng_0_engtype_3d",
                70.0,
            )
            .unwrap(),
            parse_gpu_engine_instance(
                "pid_1488_luid_0x00000000_0x0000aaaa_phys_0_eng_0_engtype_3d",
                20.0,
            )
            .unwrap(),
        ];
        let (luid_shared, game_shared) = aggregate_gpu_utilization(&shared, 5000);
        assert!((luid_shared.unwrap() - 90.0).abs() < 0.01);
        assert!((game_shared.unwrap() - 70.0).abs() < 0.01);
    }

    #[test]
    fn test_process_cpu_monitor_self_smoke() {
        let mut mon = ProcessCpuMonitor::new();
        let pid = std::process::id();
        let t0 = Instant::now();
        assert!(mon.sample(pid, t0).is_none());
        std::thread::sleep(Duration::from_millis(70));
        let pct = mon.sample(pid, Instant::now());
        assert!(pct.is_some());
        let val = pct.unwrap();
        assert!((0.0..=100.0).contains(&val));
    }

    #[test]
    fn test_gpu_miss_grace_holds_two_blips_then_clears() {
        let mut misses = 0;
        let (g, gg) = apply_gpu_sample_with_grace(None, None, Some(80.0), Some(75.0), &mut misses);
        assert_eq!(misses, 0);
        assert_eq!(g, Some(80.0));
        assert_eq!(gg, Some(75.0));

        // Miss #1 preserves cached EMA
        let (g1, gg1) = apply_gpu_sample_with_grace(g, gg, None, None, &mut misses);
        assert_eq!(misses, 1);
        assert_eq!(g1, Some(80.0));
        assert_eq!(gg1, Some(75.0));

        // Miss #2 preserves cached EMA
        let (g2, gg2) = apply_gpu_sample_with_grace(g1, gg1, None, None, &mut misses);
        assert_eq!(misses, 2);
        assert_eq!(g2, Some(80.0));
        assert_eq!(gg2, Some(75.0));

        // Recovery before miss #3 resets misses and smooths new sample
        let (g_rec, gg_rec) =
            apply_gpu_sample_with_grace(g2, gg2, Some(90.0), Some(85.0), &mut misses);
        assert_eq!(misses, 0);
        assert!(g_rec.is_some());
        assert!(gg_rec.is_some());

        // Three consecutive misses clear cached GPU to None
        let (m1, mm1) = apply_gpu_sample_with_grace(g_rec, gg_rec, None, None, &mut misses);
        let (m2, mm2) = apply_gpu_sample_with_grace(m1, mm1, None, None, &mut misses);
        let (m3, mm3) = apply_gpu_sample_with_grace(m2, mm2, None, None, &mut misses);
        assert_eq!(misses, 3);
        assert_eq!(m3, None);
        assert_eq!(mm3, None);
    }
}
