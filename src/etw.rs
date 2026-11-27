use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::Performance::*;
use windows::core::*;

/// Provider GUIDs
pub const DXGI_PROVIDER_GUID: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
pub const D3D9_PROVIDER_GUID: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);

static GLOBAL_FRAME_RECORDER: OnceLock<Arc<Mutex<FrameRecorder>>> = OnceLock::new();

pub struct FrameRecorder {
    /// Maps PID -> queue of frame present QPC timestamps
    pids: HashMap<u32, VecDeque<i64>>,
    qpc_frequency: i64,
}

impl Default for FrameRecorder {
    fn default() -> Self {
        let mut freq: i64 = 10_000_000;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut freq);
        }
        if freq <= 0 {
            freq = 10_000_000;
        }
        Self {
            pids: HashMap::new(),
            qpc_frequency: freq,
        }
    }
}

impl FrameRecorder {
    pub fn record_present(&mut self, pid: u32, qpc_timestamp: i64) {
        let queue = self.pids.entry(pid).or_default();
        queue.push_back(qpc_timestamp);
        // Retain last 1000 frames per PID (~4s at 240 FPS) to avoid unbounded growth
        if queue.len() > 1000 {
            queue.pop_front();
        }
    }

    pub fn current_qpc() -> i64 {
        let mut now: i64 = 0;
        unsafe {
            let _ = QueryPerformanceCounter(&mut now);
        }
        now
    }

    fn extract_intervals(
        &self,
        queue: &VecDeque<i64>,
        cutoff_qpc: i64,
        warmup_ticks: i64,
    ) -> Vec<f64> {
        let mut intervals = Vec::new();
        let mut prev: Option<i64> = None;
        let mut warmup_until: Option<i64> = None;

        for &t in queue {
            if t >= cutoff_qpc {
                let start = *warmup_until.get_or_insert_with(|| t.saturating_add(warmup_ticks));
                if t >= start
                    && let Some(p) = prev
                    && p >= start
                {
                    let dt_ticks = t.saturating_sub(p);
                    let dt_ms = (dt_ticks as f64 * 1000.0) / (self.qpc_frequency as f64);
                    if dt_ms > 0.5 && dt_ms < 1000.0 {
                        intervals.push(dt_ms);
                    }
                }
            }
            prev = Some(t);
        }
        intervals
    }

    /// Computes frame intervals (in ms) for the most recent frames relative to the last recorded frame.
    pub fn get_frame_intervals(&self, pid: u32, window: Duration) -> Vec<f64> {
        let queue = match self.pids.get(&pid) {
            Some(q) if q.len() >= 2 => q,
            _ => return Vec::new(),
        };

        let last_qpc = match queue.back() {
            Some(&t) => t,
            None => return Vec::new(),
        };

        let window_ticks = ((window.as_secs_f64()) * (self.qpc_frequency as f64)) as i64;
        let cutoff_qpc = last_qpc.saturating_sub(window_ticks);
        self.extract_intervals(queue, cutoff_qpc, 0)
    }

    /// Computes frame intervals (in ms) within `window` relative to current wall-clock QPC.
    /// Returns empty if the game has stopped presenting (loading screen, alt-tab, freeze).
    pub fn get_frame_intervals_recent(&self, pid: u32, window: Duration) -> Vec<f64> {
        self.get_frame_intervals_recent_at(pid, window, Self::current_qpc(), Duration::ZERO)
    }

    /// Computes frame intervals relative to an explicit `now_qpc`, optionally discarding `warmup` after the first frame in window.
    pub fn get_frame_intervals_recent_at(
        &self,
        pid: u32,
        window: Duration,
        now_qpc: i64,
        warmup: Duration,
    ) -> Vec<f64> {
        let queue = match self.pids.get(&pid) {
            Some(q) if q.len() >= 2 => q,
            _ => return Vec::new(),
        };

        let window_ticks = ((window.as_secs_f64()) * (self.qpc_frequency as f64)) as i64;
        let warmup_ticks = ((warmup.as_secs_f64()) * (self.qpc_frequency as f64)) as i64;
        let cutoff_qpc = now_qpc.saturating_sub(window_ticks);
        self.extract_intervals(queue, cutoff_qpc, warmup_ticks)
    }

    /// Computes current FPS for `pid` over the specified window relative to the last recorded frame.
    pub fn get_current_fps(&self, pid: u32, window: Duration) -> Option<f64> {
        let intervals = self.get_frame_intervals(pid, window);
        if intervals.len() < 3 {
            return None;
        }
        let total_ms: f64 = intervals.iter().sum();
        if total_ms <= 0.0 {
            return None;
        }
        let avg_frame_ms = total_ms / (intervals.len() as f64);
        Some(1000.0 / avg_frame_ms)
    }

    /// Computes live FPS for `pid` over `window` relative to current QPC (returns `None` when idle/stalled).
    pub fn get_current_fps_recent(&self, pid: u32, window: Duration) -> Option<f64> {
        let intervals = self.get_frame_intervals_recent(pid, window);
        if intervals.len() < 3 {
            return None;
        }
        let total_ms: f64 = intervals.iter().sum();
        if total_ms <= 0.0 {
            return None;
        }
        Some(1000.0 / (total_ms / intervals.len() as f64))
    }

    /// Computes trimmed-mean FPS over `window`, discarding initial `warmup` transition frames and 5% top/bottom outliers.
    pub fn get_trimmed_fps_recent(
        &self,
        pid: u32,
        window: Duration,
        warmup: Duration,
    ) -> Option<f64> {
        self.get_trimmed_fps_recent_at(pid, window, Self::current_qpc(), warmup)
    }

    pub fn get_trimmed_fps_recent_at(
        &self,
        pid: u32,
        window: Duration,
        now_qpc: i64,
        warmup: Duration,
    ) -> Option<f64> {
        let mut intervals = self.get_frame_intervals_recent_at(pid, window, now_qpc, warmup);
        if intervals.len() < 3 {
            return None;
        }
        intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let slice = if intervals.len() >= 10 {
            let trim = ((intervals.len() as f64) * 0.05).floor() as usize;
            if trim * 2 < intervals.len() {
                &intervals[trim..intervals.len() - trim]
            } else {
                &intervals[..]
            }
        } else {
            &intervals[..]
        };
        let total_ms: f64 = slice.iter().sum();
        if total_ms <= 0.0 || slice.is_empty() {
            return None;
        }
        Some(1000.0 / (total_ms / slice.len() as f64))
    }

    pub fn clear_pid(&mut self, pid: u32) {
        self.pids.remove(&pid);
    }
}

pub fn get_global_recorder() -> Arc<Mutex<FrameRecorder>> {
    GLOBAL_FRAME_RECORDER
        .get_or_init(|| Arc::new(Mutex::new(FrameRecorder::default())))
        .clone()
}

unsafe extern "system" fn event_record_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let rec = unsafe { &*record };
    let provider = rec.EventHeader.ProviderId;
    let event_id = rec.EventHeader.EventDescriptor.Id;

    // Check if this is a Present event (DXGI Present_Start = 42, MPO Present_Start = 55; D3D9 = 1)
    let is_present = if provider == DXGI_PROVIDER_GUID {
        event_id == 42 || event_id == 55
    } else if provider == D3D9_PROVIDER_GUID {
        event_id == 1
    } else {
        false
    };

    if is_present {
        let pid = rec.EventHeader.ProcessId;
        let ts = rec.EventHeader.TimeStamp;
        if pid > 4
            && let Ok(mut rec) = get_global_recorder().lock()
        {
            rec.record_present(pid, ts);
        }
    }
}

pub struct EtwTraceSession {
    session_handle: CONTROLTRACE_HANDLE,
    _session_name: Vec<u16>,
    trace_handle: PROCESSTRACE_HANDLE,
    _logfile: Box<EVENT_TRACE_LOGFILEW>,
    worker_thread: Option<std::thread::JoinHandle<()>>,
}

unsafe impl Send for EtwTraceSession {}

#[repr(C)]
struct TracePropertiesBuffer {
    props: EVENT_TRACE_PROPERTIES,
    logger_name: [u16; 256],
    logfile_name: [u16; 256],
}

impl EtwTraceSession {
    pub fn start(session_name_str: &str) -> std::result::Result<Self, String> {
        let mut session_name: Vec<u16> = session_name_str.encode_utf16().collect();
        session_name.push(0);

        let mut buf: TracePropertiesBuffer = unsafe { std::mem::zeroed() };
        let buf_size = std::mem::size_of::<TracePropertiesBuffer>() as u32;

        buf.props.Wnode.BufferSize = buf_size;
        buf.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        buf.props.Wnode.ClientContext = 1; // QPC clock
        buf.props.BufferSize = 64;
        buf.props.MinimumBuffers = 2;
        buf.props.MaximumBuffers = 8;
        buf.props.FlushTimer = 1; // Flush buffers every 1 second
        buf.props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        buf.props.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        buf.props.LogFileNameOffset = 0;

        let mut session_handle = CONTROLTRACE_HANDLE::default();

        // If an old session is still running, stop it first
        unsafe {
            let _ = ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(session_name.as_ptr()),
                &mut buf.props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }

        // Completely re-zero and reset buffer for StartTraceW
        buf = unsafe { std::mem::zeroed() };
        buf.props.Wnode.BufferSize = buf_size;
        buf.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        buf.props.Wnode.ClientContext = 1;
        buf.props.BufferSize = 64;
        buf.props.MinimumBuffers = 2;
        buf.props.MaximumBuffers = 8;
        buf.props.FlushTimer = 1;
        buf.props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        buf.props.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        buf.props.LogFileNameOffset = 0;

        unsafe {
            let status = StartTraceW(
                &mut session_handle,
                PCWSTR(session_name.as_ptr()),
                &mut buf.props,
            );
            if status.is_err() {
                return Err(format!("StartTraceW failed: {:?}", status));
            }

            // Enable DXGI provider with verbose level (5) and all keywords (u64::MAX)
            // to ensure Present events (Id 42 / 55) in the Analytic/Logging channels are delivered
            let dxgi_status = EnableTraceEx2(
                session_handle,
                &DXGI_PROVIDER_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                5,
                u64::MAX,
                0,
                0,
                None,
            );
            if dxgi_status.is_err() {
                let _ = ControlTraceW(
                    session_handle,
                    PCWSTR::null(),
                    &mut buf.props,
                    EVENT_TRACE_CONTROL_STOP,
                );
                return Err(format!("EnableTraceEx2(DXGI) failed: {:?}", dxgi_status));
            }

            // Enable D3D9 provider
            let d3d9_status = EnableTraceEx2(
                session_handle,
                &D3D9_PROVIDER_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                5,
                u64::MAX,
                0,
                0,
                None,
            );
            if d3d9_status.is_err() {
                crate::logger::log_warn(format!(
                    "EnableTraceEx2(D3D9) failed: {:?} (DXGI still active)",
                    d3d9_status
                ));
            }

            // Open realtime consumer trace - allocate on heap so pointer remains valid
            let mut logfile = Box::new(std::mem::zeroed::<EVENT_TRACE_LOGFILEW>());
            logfile.LoggerName = PWSTR(session_name.as_mut_ptr());
            logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
                | PROCESS_TRACE_MODE_EVENT_RECORD
                | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
            logfile.Anonymous2.EventRecordCallback = Some(event_record_callback);

            let trace_handle = OpenTraceW(&mut *logfile);
            if trace_handle.Value == 0 || trace_handle.Value == u64::MAX {
                let _ = ControlTraceW(
                    session_handle,
                    PCWSTR::null(),
                    &mut buf.props,
                    EVENT_TRACE_CONTROL_STOP,
                );
                return Err("OpenTraceW returned invalid handle".to_string());
            }

            let th = trace_handle;
            let worker = std::thread::spawn(move || {
                let handles = [th];
                let _ = ProcessTrace(&handles, None, None);
            });

            Ok(Self {
                session_handle,
                _session_name: session_name,
                trace_handle,
                _logfile: logfile,
                worker_thread: Some(worker),
            })
        }
    }

    pub fn flush(&self) {
        if self.session_handle.Value != 0 {
            unsafe {
                let mut buf: TracePropertiesBuffer = std::mem::zeroed();
                buf.props.Wnode.BufferSize = std::mem::size_of::<TracePropertiesBuffer>() as u32;
                buf.props.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
                let _ = ControlTraceW(
                    self.session_handle,
                    PCWSTR::null(),
                    &mut buf.props,
                    EVENT_TRACE_CONTROL_FLUSH,
                );
            }
        }
    }

    pub fn stop(&mut self) {
        unsafe {
            if self.trace_handle.Value != 0 && self.trace_handle.Value != u64::MAX {
                let _ = CloseTrace(self.trace_handle);
                self.trace_handle = PROCESSTRACE_HANDLE::default();
            }

            if self.session_handle.Value != 0 {
                let mut buf: TracePropertiesBuffer = std::mem::zeroed();
                buf.props.Wnode.BufferSize = std::mem::size_of::<TracePropertiesBuffer>() as u32;
                buf.props.LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
                let _ = ControlTraceW(
                    self.session_handle,
                    PCWSTR::null(),
                    &mut buf.props,
                    EVENT_TRACE_CONTROL_STOP,
                );
                self.session_handle = CONTROLTRACE_HANDLE::default();
            }
        }

        if let Some(worker) = self.worker_thread.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for EtwTraceSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_recorder_intervals_and_fps() {
        let mut rec = FrameRecorder::default();
        let pid = 12345;
        let freq = rec.qpc_frequency;

        // Simulate 60 FPS: interval = freq / 60
        let step = freq / 60;
        let mut base: i64 = 1_000_000_000;

        for _ in 0..60 {
            rec.record_present(pid, base);
            base += step;
        }

        let intervals = rec.get_frame_intervals(pid, Duration::from_secs(2));
        assert!(intervals.len() >= 55);
        for &dt in &intervals {
            assert!((dt - 16.666).abs() < 1.0);
        }

        let fps = rec.get_current_fps(pid, Duration::from_secs(2)).unwrap();
        assert!((fps - 60.0).abs() < 1.0);
    }

    #[test]
    fn test_recent_intervals_ignores_stale_frames() {
        let mut rec = FrameRecorder::default();
        let pid = 4242;
        let freq = rec.qpc_frequency;
        let step = freq / 60;
        let now_qpc = FrameRecorder::current_qpc();
        let ten_secs_ago = now_qpc - (freq * 10);

        for i in 0..60 {
            rec.record_present(pid, ten_secs_ago + i * step);
        }

        // Old get_frame_intervals returns stale frames, while _recent returns empty
        assert!(!rec.get_frame_intervals(pid, Duration::from_secs(2)).is_empty());
        assert!(rec.get_frame_intervals_recent(pid, Duration::from_secs(2)).is_empty());
        assert!(rec.get_current_fps_recent(pid, Duration::from_secs(2)).is_none());
    }

    #[test]
    fn test_trimmed_fps_recent_discards_warmup_and_outliers() {
        let mut rec = FrameRecorder::default();
        let pid = 4243;
        let freq = rec.qpc_frequency;
        let mut ts: i64 = 10_000_000_000;

        // First 250ms warmup at 30 FPS (transition frames after limiter disengage)
        let step_30 = freq / 30;
        for _ in 0..8 {
            rec.record_present(pid, ts);
            ts += step_30;
        }
        // Next 1.0s at steady 120 FPS with one huge 80ms spike
        let step_120 = freq / 120;
        for i in 0..120 {
            rec.record_present(pid, ts);
            ts += if i == 60 { step_120 * 10 } else { step_120 };
        }

        let fps = rec
            .get_trimmed_fps_recent_at(
                pid,
                Duration::from_secs_f64(1.3),
                ts,
                Duration::from_millis(260),
            )
            .expect("Expected trimmed FPS");
        assert!(
            (fps - 120.0).abs() < 3.0,
            "Expected ~120 FPS after warmup & outlier trim, got {:.2}",
            fps
        );
    }
}
