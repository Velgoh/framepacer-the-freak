# FramePacer Companion

<p align="center">
  <img src="assets/default.gif" width="240" alt="FramePacer Companion" />
</p>

An automatic, zero-overlay companion utility in Rust for Steam's **FramePacer**. It monitors live frame pacing alongside Windows GPU and per-thread CPU utilization, continuously adjusting FramePacer's FPS cap via shared-memory IPC without disabling the limiter during normal gameplay.

---

## What It Does

1. **Headroom-Guided Never-Off Pacing (`never_off`, default)**:
   - Keeps FramePacer's limiter engaged continuously while stepping/ramping the cap upward whenever the game is hitting its current cap with spare GPU/CPU headroom.
   - Steps down smoothly (`step_fps`, default `5`) when sustained late frames indicate a real hardware or engine limit, applying a cooldown so it doesn't bounce back and forth at the GPU wall.
2. **Cutscene & Software-Lock Detection**:
   - Detects sudden FPS drops (`JumpTo`) vs. heavy load drops, and distinguishes self-capped cutscenes / in-game FPS limits / VSync (`low GPU + pinned flat FPS`) from real hardware bottlenecks.
   - Uses invisible `+step_fps` re-probes during software locks so recovery back to unlocked gameplay happens within seconds instead of stalling on a 2-minute ceiling timer.
3. **Multi-Scenario Safeguards**:
   - **Alt-Tab / Minimized Pause**: Detects when FramePacer marks the process inactive or the game window is minimized, freezing cap adjustments until the game returns to focus.
   - **Auto-Detected Log Path**: Resolves `framepacer.exe`'s directory from IPC (`0x00C`) if Steam/FramePacer isn't installed on `C:\`.
   - **Portable Borderless Window & Custom GIF Picker**: Hover the window to view live telemetry (`IPC` / `ETW`, attached process, status, measured FPS, `G<nn>%`), pause/resume (`F11` hotkey or button), minimize/close, or swap in a custom `.gif` via URL, share page link, file browser, or drag-and-drop (window size and accent colors adapt automatically).

---

## Requirements

- **OS**: Windows 10 / 11 x64
- **Dependency**: Steam **FramePacer** (`v0.6.0.0` IPC v5 protocol) running on the system
- **Privileges**: Administrator elevation (manifested automatically on `FramePacerCompanion.exe` for kernel ETW session management and FramePacer IPC access)

---

## Architecture

```
+-----------------------------------------------------------------------+
|                         FramePacer Companion                          |
|                                                                       |
|  +----------------------+   0x028 / 0x02C   +----------------------+  |
|  |  Engine Controller   | ----------------> | Local\FramepacerIPC  |  |
|  |  (Never-Off / Pulse  |                   |      _v5 (1256B)     |  |
|  |   State Machine)     | <---------------- |  (Win32 Shared Mem)  |  |
|  +----------------------+  0x038 Ring / PID +----------------------+  |
|        ^          ^                                    |              |
|        |          | Frametimes                         v              |
|  +-----+----+ +---+-------------------+    +-----------------------+  |
|  | Windows  | |     ETW Consumer      |    |      FramePacer       |  |
|  | PDH GPU  | | DXGI (42, 55) & D3D9  |    |   (framepacer.dll)    |  |
|  | & Thread | +-----------------------+    +-----------------------+  |
|  | CPU Mon  |             ^                            |              |
|  +----------+             | Present QPC                v              |
|                    +------+----------------------------+------+       |
|                    |        Game Process (D3D9/11/12)         |       |
|                    +------------------------------------------+       |
+-----------------------------------------------------------------------+
```

### Shared Memory (`Local\FramepacerIPC_v5`, 1256 bytes)

| Offset | Type | Description |
|---|---|---|
| `0x000` | `u32` | Magic (`0x43504946`, `"FIPC"`) |
| `0x004` | `u32` | Protocol version (`5`) |
| `0x008` | `u32` | Section size (`1256`) |
| `0x00C` | `u32` | FramePacer owner PID |
| `0x020` | `u32` | IPC active flag (`1` = active, `0xDEAD` = closed) |
| `0x028` | `u32` | Target FPS in millihertz (`fps * 1000`) |
| `0x02C` | `u32` | Limiter enabled flag (`1` = ON, `0` = OFF) |
| `0x030` | `u32` | Frametime ring write index (monotonic) |
| `0x034` | `u32` | Target process active state (`2` = inactive) |
| `0x038` | `f64[120]` | Live frametime ring buffer (ms), guarded by seqlock |
| `0x408` | `u32` | In-game DLL attached flag |
| `0x420` | `u32` | Attached game PID |
| `0x4DC` | `u32` | Seqlock counter for ring & API string reads |

### Telemetry Sources

- **Frametimes**: Prefers whichever live source (`ETW` DXGI events `42`/`55` + D3D9 event `1`, or FramePacer's `0x038` IPC frametime ring sliced to the post-settle window) provides more frames.
- **GPU Utilization**: Vendor-agnostic Windows PDH `\GPU Engine(*)\Utilization Percentage` across `3d`, `graphics*`, and `compute*` engines on the game's active GPU LUID, with brief-blip tolerance and automatic query recovery.
- **CPU Utilization**: Per-thread CPU saturation via `ToolHelp` + `GetThreadTimes` to detect single-thread CPU bottlenecks even when total CPU usage looks low.

---

## Configuration (`data/config.toml`)

```toml
pacing_mode = "never_off"          # "never_off" or "pulse"
target_gpu_util_pct = 88.0         # Target GPU % used for headroom jump sizing
hold_gpu_util_pct = 88.0           # Above this %, avoid speculative upward probes
scale_up_gpu_util_pct = 80.0       # Below this %, scale up toward target GPU load
max_cpu_thread_util_pct = 85.0     # Peak single-thread CPU % limit
probe_verify_ms = 500              # Upward probe verification window (ms)
wakeup_nudge_interval_secs = 45.0  # Hold-zone nudge interval
min_fps = 30
max_fps = 200
step_fps = 5
stable_recheck_interval_secs = 30.0 # Pulse mode only
measure_duration_secs = 1.0         # Pulse mode only
drop_recheck_interval_secs = 4.0    # Pulse mode only
spike_tolerance_ms = 2.0
late_frame_threshold_ratio = 0.05
drop_detection_fps = 15
hotkey_settle_ms = 80
framepacer_log_path = "C:\\Program Files (x86)\\Steam\\steamapps\\common\\framepacer\\framepacer.log"
exclude_processes = []
auto_enabled_at_startup = true
```

---

## Logs & Troubleshooting

Runtime logs are written to `data/framepacer_companion.log` (rotated automatically at 2 MB on startup), including:
- `[CONFIG]` startup parameters, elevation status, ETW/PDH initialization, and IPC connection status
- `[ATTACH]` / `[DETACH]` / `[BACKGROUND]` / `[FOREGROUND]` game lifecycle transitions
- `[SCALE UP]` / `[PROBE PASS]` / `[PROBE REVERT]` / `[STEP]` / `[CUTSCENE]` / `[SNAP]` decisions with measured FPS, sample count, metric source (`ipc`/`etw`), GPU %, and peak CPU thread %
- 60-second `[STATUS]` heartbeat summaries while a game is attached

---

## Build

### Prerequisites
- Windows 10 / 11 x64
- Rust toolchain (edition 2024 / `rustc 1.85+`)

### Commands
```powershell
cargo test
cargo clippy --all-targets -- -D warnings
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

Packaged output is generated in `dist/` containing `FramePacerCompanion.exe`, `data/`, and `FramePacerCompanion-v0.3.0.zip`.

---

## Support

Glory to mankind.
