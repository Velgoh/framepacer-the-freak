# THE FREAK

## Technical Architecture

```
+------------------------------------------------------------------+
|                       framepacer-the-freak                       |
|                                                                  |
|   +-------------------+   IPC Writes   +---------------------+   |
|   | Engine Controller | -------------> | Local\              |   |
|   |  (Pulse Benchmark |                | FramepacerIPC_v5    |   |
|   |   State Machine)  |                | (Win32 MMF)         |   |
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

The system maps the Win32 file mapping object named `Local\FramepacerIPC_v5` with `FILE_MAP_READ | FILE_MAP_WRITE` permissions.

### Shared Memory Section Layout

| Offset (Hex) | Offset (Dec) | Type  | Description |
|---|---|---|---|
| `0x000` | 0 | `u32` | Header Magic: `0x43504946` (`"FIPC"`) |
| `0x004` | 4 | `u32` | Protocol Version (`5`) |
| `0x008` | 8 | `u32` | Section Allocation Size (`1256` bytes) |
| `0x020` | 32 | `u32` | IPC Active Status (`1` = active) |
| `0x028` | 40 | `u32` | Target Framerate in millihertz (`fps * 1000`, e.g. `60000` for 60.0 FPS) |
| `0x02C` | 44 | `u32` | Limiter State (`1` = enabled, `0` = disabled) |
| `0x030` | 48 | `u32` | Display Refresh Rate in millihertz |
| `0x034` | 52 | `u32` | Limiter Execution Mode (`2` = normal) |
| `0x408` | 1032 | `u32` | In-Game Hook Attachment Flag (`1` = attached) / Mirrored Limiter Flag |
| `0x40C` | 1036 | `u32` | Target Game Process ID |
| `0x414` | 1044 | `u32` | Mirrored Target Framerate Cap (direct integer) |
| `0x420` | 1056 | `u32` | In-Game Hook Active Process ID |

### Atomic Read/Write Semantics
- Memory writes execute via volatile atomic stores with sequential consistency memory fences (`fence(Ordering::SeqCst)`).
- Limiter disengagement and re-engagement write directly to offset `0x02C`.
- Target FPS modifications update native millihertz at `0x028` and direct cap integer at `0x414`.

### Live Shared Memory Polling
Every 100ms cycle, offset `0x028` / `0x414` is read. If the value differs from the internal active cap state (e.g. an external modification via FramePacer's GUI), the engine immediately synchronizes its internal state, adopts the new cap, and resets the settling timer.

---

## Real-Time ETW Metric Acquisition

Frametimes and delivery intervals are measured through kernel Event Tracing for Windows (ETW) without code injection:

- **Primary Provider**: `Microsoft-Windows-DXGI` (`{CA11C036-0102-4A2D-A6AD-F03CFED5D3C9}`)
- **Event Identifier**: `42` (`DXGIPresent_Start`)
- **Secondary Provider**: `Microsoft-Windows-D3D9` (`{783ACA0A-790E-4D7F-8451-AA850511C6B9}`)
- **Event Identifier**: `1`

Timestamps are extracted from `EVENT_HEADER.TimeStamp` as 64-bit QueryPerformanceCounter (QPC) ticks. Frametime intervals ($\Delta t$) are computed relative to the system performance counter frequency ($QPC_{freq}$):

$$\Delta t_{ms} = \frac{\Delta QPC \times 1000}{QPC_{freq}}$$

A per-PID ring buffer maintains a 2.5-second sliding window of frame timestamps.

---

## Pulse-Benchmarking State Machine

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
                                | Settle timer expired  |
                                v                       |
               +----------------------------------+     | Cap changed
+------------> |           PacingActive           | ----+
|              |  (Limiter ON: Continuous Eval)   |
|              +----------------------------------+
|                |              |              |
| KeepCap        | 30s Recheck  | StepDown (-5)| JumpTo (30)
| (Smooth)       | Elapsed      | (Late ratio) | (Delta >= 15 FPS)
+----------------+              v              v
                        +---------------+
                        |MeasuringNative|
                        | (1.0s Limiter |
                        |     OFF)      |
                        +---------------+
                                | 1.0s expired -> read ETW native FPS
                                | snap cap down to 5-FPS boundary
                                | turn limiter back ON
                                v
                        [Trigger Settle]
```

### Execution Rules

1. **30-Second Periodic Benchmark Pulse**:
   - Every 30.0 seconds of stable pacing (`stable_recheck_interval_secs`), the engine transitions to `AutoState::MeasuringNative`.
   - Limiter is disengaged (`0x02C = 0`).
   - Frame intervals are recorded uncapped for exactly 1.0 second (`measure_duration_secs = 1.0`).
   - Native uncapped FPS is calculated from the measured interval set:
     $$FPS_{native} = \frac{N \times 1000}{\sum_{i=1}^{N} \Delta t_i}$$
   - Target cap is snapped down to the nearest 5-FPS boundary:
     $$Cap = \left\lfloor \frac{FPS_{native} + 0.5}{5} \right\rfloor \times 5$$
   - The computed cap is written to `0x028` and `0x414`.
   - Limiter is immediately re-engaged (`0x02C = 1`).
   - Engine enters `AutoState::Settling` for 1000ms.

2. **Immediate Downward Cadence Reaction**:
   - If late frame ratio exceeds `0.05` or $\ge 3$ consecutive late frames occur during `PacingActive`, cap decrements by 5 FPS immediately.
   - If rendering drops by $\ge 15$ FPS relative to cap (e.g. 30 FPS cutscene), engine snaps cap directly to 30 FPS.
   - While in drop state, recheck interval is accelerated to 4.0 seconds (`drop_recheck_interval_secs`) to detect gameplay resumption.

3. **Live Cap Synchronization**:
   - Polling occurs every 100ms. External modifications to `0x028` update the active cap immediately without fighting or reverting user input.

---

## Window Geometry & Persistence

- **Dimensions**: Viewport is initialized to `300 x 228` pixels (aspect ratio 1.315:1 matching source visual assets).
- **Decorations**: System borders and titlebars are disabled (`with_decorations(false)`).
- **Z-Order**: Configured with `with_always_on_top(true)`.
- **Position Storage**: Window coordinates are loaded from and saved to `data/window.json`:
  ```json
  {
    "x": 300.0,
    "y": 200.0
  }
  ```
- **Pointer Gating**: Interactive glyphs (auto sigil, minimize, close) are rendered exclusively when the pointer hovers inside the window area (`hover_pos().is_some()`).

---

## Configuration (`config.toml`)

```toml
min_fps = 30
max_fps = 200
step_fps = 5
stable_recheck_interval_secs = 30.0
measure_duration_secs = 1.0
drop_recheck_interval_secs = 4.0
spike_tolerance_ms = 2.0
late_frame_threshold_ratio = 0.05
drop_detection_fps = 15
hotkey_settle_ms = 80
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

## Support

Glory to mankind.
