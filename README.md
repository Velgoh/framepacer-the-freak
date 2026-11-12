# FramePacer: The Freak

An autonomous frame-pacing companion for FramePacer. Regulates target framerate caps in real-time through direct Win32 shared memory inter-process communication (IPC) and kernel-level Event Tracing for Windows (ETW), without modifying FramePacer or injecting DLLs into target game processes.

---

## Architecture Overview

```
+------------------------------------------------------------------+
|                       framepacer-the-freak                       |
|                                                                  |
|   +-------------------+   IPC Writes   +---------------------+   |
|   | Engine Controller | -------------> | Local\              |   |
|   |  (Never-Off State |                | FramepacerIPC_v5    |   |
|   |   Machine, 100ms) |                | (Win32 MMF)         |   |
|   +-------------------+                +---------------------+   |
|             ^                                     |              |
|             | Frame intervals                     v              |
|   +-------------------+                +---------------------+   |
|   |   ETW Consumer    |                |     FramePacer      |   |
|   | (DXGI Present 42) |                | (Injection Limiter) |   |
|   +-------------------+                +---------------------+   |
|             ^                                     |              |
|             | Present QPC                         v              |
|   +-------------------+                +---------------------+   |
|   |   Game Process    | <------------- |   framepacer.dll    |   |
|   | (D3D11/12/DXGI)   |     Pacing     |  (In-game hook)     |   |
|   +-------------------+                +---------------------+   |
+------------------------------------------------------------------+
```

---

## Inter-Process Communication (`Local\FramepacerIPC_v5`)

The companion synchronizes directly with FramePacer via a Win32 file mapping object named `Local\FramepacerIPC_v5`.

### Shared Memory Layout

| Offset (Hex) | Offset (Dec) | Type  | Description |
|---|---|---|---|
| `0x000` | 0 | `u32` | Header Magic: `0x43504946` (`"FIPC"`) |
| `0x004` | 4 | `u32` | IPC Protocol Version (`5`) |
| `0x008` | 8 | `u32` | Section Size (`1256` bytes) |
| `0x020` | 32 | `u32` | IPC Active Flag (`1` = active) |
| `0x028` | 40 | `u32` | Target FPS in millihertz (`fps * 1000`, e.g., `60000` for 60.0 FPS) |
| `0x02C` | 44 | `u32` | Limiter Enabled Flag (`1` = enabled, `0` = disabled) |
| `0x030` | 48 | `u32` | Display Refresh Rate in millihertz |
| `0x034` | 52 | `u32` | Limiter Mode |
| `0x408` | 1032 | `u32` | In-game DLL Attached Status / Mirrored Limiter Flag |
| `0x40C` | 1036 | `u32` | Target Process ID |
| `0x414` | 1044 | `u32` | Mirrored Target FPS (Direct integer cap) |
| `0x420` | 1056 | `u32` | DLL Target Process ID |

IPC writes execute via volatile atomic stores with sequential consistency fences (`fence(Ordering::SeqCst)`), completing in under 15 microseconds per adjustment without queuing synthetic keyboard inputs.

---

## ETW Present Hooking

Framerate and frame interval metrics are acquired without process injection by consuming real-time kernel Event Tracing for Windows (ETW) events:

- **Provider GUID**: `Microsoft-Windows-DXGI` (`{CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}`)
- **Event ID**: `42` (`DXGIPresent_Start`)
- **Secondary Provider**: `Microsoft-Windows-D3D9` (`{783ACA0A-790E-4D7F-8451-AA850511C6B9}`)

Timestamps are captured as QueryPerformanceCounter (QPC) values from `EVENT_HEADER.TimeStamp` and stored in a fixed ring buffer per process ID. Instantaneous frametime intervals ($\Delta t$) and rolling FPS are computed over a 2.5-second sliding window.

---

## Never-Off Auto-Pacing State Machine

The pacing limiter remains engaged at all times. Native frequency sampling with the limiter disabled has been eliminated to prevent frame delivery disruption.

```
                         +--------------+
                         |     Idle     |
                         +--------------+
                                | Game Attached / Auto ON
                                v
               +----------------------------------+
               |             Settling             | <---+
               | (Cooldown window: 1000 - 1500ms) |     |
               +----------------------------------+     |
                                | Settle timer expired  | Cap changed
                                v                       |
               +----------------------------------+     |
+------------> |           PacingActive           | ----+
|              |     (Continuous In-Band Eval)    |
|              +----------------------------------+
|                |              |              |
| KeepCap        | ProbeUp (+5) | StepDown (-5)| JumpTo (30)
| (Smooth)       | (Flat >= 8s) | (Late ratio) | (Delta >= 15 FPS)
+----------------+              v              v
                        [Trigger Settle]  [Trigger Settle]
```

### Decision Rules

1. **Step Down**: If late frame ratio exceeds threshold (`> 5%`) or $\ge 3$ consecutive late frames occur, the cap decrements by `step_fps` (default: 5 FPS), bounded by `min_fps` (30 FPS).
2. **Cutscene Snap**: If sustained rendering drops by $\ge 15$ FPS relative to the cap, the engine snaps immediately to 30 FPS (`in_drop_state = true`).
3. **In-Band Probe**: When frametimes remain flat with no consecutive late frames for $\ge 8.0$ seconds (or $\ge 4.0$ seconds when recovering from a cutscene), the engine increments the cap by $+5$ FPS up to `max_fps`. If the probe causes frame drops, the step-down rule immediately restores the previous cap.

---

## User Interface

Desktop dashboard constructed in 100% Rust utilizing `eframe` and `egui`:

- Always-on-Top floating HUD (`with_always_on_top`).
- Real-time telemetry: Target FPS cap, live DXGI ETW measured FPS, active game PID, and state machine cadence.
- Embedded frame-accurate playback of `kaine-cry.gif` decoded via the `image` crate.
- Global hotkey listener: `Alt + Shift + A` via `GetAsyncKeyState`.

---

## Configuration (`config.toml`)

```toml
min_fps = 30
max_fps = 200
step_fps = 5
stable_recheck_interval_secs = 8.0
drop_recheck_interval_secs = 4.0
spike_tolerance_ms = 2.0
late_frame_threshold_ratio = 0.05
drop_detection_fps = 15
framepacer_log_path = "C:\\Program Files (x86)\\Steam\\steamapps\\common\\framepacer\\framepacer.log"
exclude_processes = []
auto_enabled_at_startup = true
```

---

## Build

### Prerequisites
- Windows 10 / 11 x64
- Rust toolchain (edition 2024 / 1.85+)

### Compilation
```powershell
cargo build --release
```

Release artifact generated at:
```
target/release/framepacer-the-freak.exe
```

---

*Glory to mankind.*
