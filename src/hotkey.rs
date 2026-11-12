use std::thread::sleep;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub const COMPANION_HOTKEY_ID: i32 = 101;
pub const COMPANION_HOTKEY_VK: u32 = 0x41; // 'A'

/// Registers the companion's global toggle hotkey (Alt+Shift+A) with clean message consumption.
pub fn register_companion_hotkey(hwnd: HWND, id: i32) -> windows::core::Result<()> {
    unsafe {
        RegisterHotKey(
            Some(hwnd),
            id,
            HOT_KEY_MODIFIERS(MOD_ALT.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0),
            COMPANION_HOTKEY_VK,
        )
    }
}

/// Unregisters the companion's global toggle hotkey.
pub fn unregister_companion_hotkey(hwnd: HWND, id: i32) -> windows::core::Result<()> {
    unsafe { UnregisterHotKey(Some(hwnd), id) }
}

pub fn is_key_physically_down(vk: VIRTUAL_KEY) -> bool {
    unsafe {
        // High-order bit indicates whether the key is currently pressed
        (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0
    }
}

/// Checks whether any hotkey-relevant key is physically held down by the user.
/// We guard Alt, Shift, Ctrl, Up, Down, L, A.
pub fn is_any_guarded_key_down() -> bool {
    let keys = [
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_UP,
        VK_DOWN,
        VIRTUAL_KEY(0x4C), // 'L'
        VIRTUAL_KEY(0x41), // 'A'
    ];
    for &k in &keys {
        if is_key_physically_down(k) {
            return true;
        }
    }
    false
}

/// Waits until no guarded keys are physically held, up to `timeout`.
pub fn wait_for_keys_released(timeout: Duration) -> bool {
    let start = Instant::now();
    while is_any_guarded_key_down() {
        if start.elapsed() >= timeout {
            return false;
        }
        sleep(Duration::from_millis(10));
    }
    true
}

fn make_key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    // Extended key flag for arrows
    if vk == VK_UP || vk == VK_DOWN || vk == VK_LEFT || vk == VK_RIGHT {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Deprecated: Sends a chord of keys down and up.
/// Replaced by direct Shared Memory IPC in `src/ipc.rs` to avoid disrupting gameplay controls.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc in src/ipc.rs instead."
)]
pub fn send_chord(keys: &[VIRTUAL_KEY], settle_ms: u64) -> Result<(), &'static str> {
    if !wait_for_keys_released(Duration::from_millis(500)) {
        return Err("Guarded keys physically held by user; skipping hotkey injection");
    }

    let mut inputs_down = Vec::with_capacity(keys.len());
    for &k in keys {
        inputs_down.push(make_key_input(k, false));
    }

    let mut inputs_up = Vec::with_capacity(keys.len());
    for &k in keys.iter().rev() {
        inputs_up.push(make_key_input(k, true));
    }

    unsafe {
        let sent_down = SendInput(&inputs_down, std::mem::size_of::<INPUT>() as i32);
        if sent_down != inputs_down.len() as u32 {
            return Err("Failed to send key-down chord");
        }
    }

    sleep(Duration::from_millis(settle_ms.clamp(10, 100)));

    unsafe {
        let sent_up = SendInput(&inputs_up, std::mem::size_of::<INPUT>() as i32);
        if sent_up != inputs_up.len() as u32 {
            return Err("Failed to send key-up chord");
        }
    }

    sleep(Duration::from_millis(settle_ms.clamp(10, 100)));
    Ok(())
}

/// Deprecated: Alt+Shift+L: FramePacer toggle limiter.
/// Replaced by `FramepacerIpc::set_limiter_enabled`.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc::set_limiter_enabled instead."
)]
#[allow(deprecated)]
pub fn send_toggle_limiter(settle_ms: u64) -> Result<(), &'static str> {
    send_chord(&[VK_MENU, VK_SHIFT, VIRTUAL_KEY(0x4C)], settle_ms)
}

/// Deprecated: Alt+Shift+Up: FramePacer +5 FPS.
/// Replaced by `FramepacerIpc::set_target_fps`.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc::set_target_fps instead."
)]
#[allow(deprecated)]
pub fn send_step_up(settle_ms: u64) -> Result<(), &'static str> {
    send_chord(&[VK_MENU, VK_SHIFT, VK_UP], settle_ms)
}

/// Deprecated: Alt+Shift+Down: FramePacer -5 FPS.
/// Replaced by `FramepacerIpc::set_target_fps`.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc::set_target_fps instead."
)]
#[allow(deprecated)]
pub fn send_step_down(settle_ms: u64) -> Result<(), &'static str> {
    send_chord(&[VK_MENU, VK_SHIFT, VK_DOWN], settle_ms)
}

/// Deprecated: Steps cap using keyboard pulses.
/// Replaced by direct memory write via `FramepacerIpc::set_target_fps`.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc::set_target_fps instead."
)]
pub fn send_steps(vk: VIRTUAL_KEY, count: u32, step_delay_ms: u64) -> Result<(), &'static str> {
    if count == 0 {
        return Ok(());
    }
    if !wait_for_keys_released(Duration::from_millis(500)) {
        return Err("Guarded keys physically held by user; skipping hotkey injection");
    }

    let alt = VK_MENU;
    let shift = VK_SHIFT;
    let delay = Duration::from_millis(step_delay_ms.clamp(20, 60));

    // Send Alt down, Shift down
    let mods_down = [make_key_input(alt, false), make_key_input(shift, false)];
    unsafe {
        let sent = SendInput(&mods_down, std::mem::size_of::<INPUT>() as i32);
        if sent != mods_down.len() as u32 {
            return Err("Failed to send modifier key-down chord");
        }
    }
    sleep(delay);

    for _ in 0..count {
        let key_down = [make_key_input(vk, false)];
        let key_up = [make_key_input(vk, true)];
        unsafe {
            let sent_d = SendInput(&key_down, std::mem::size_of::<INPUT>() as i32);
            if sent_d != 1 {
                return Err("Failed to send step key-down");
            }
        }
        sleep(delay);
        unsafe {
            let sent_u = SendInput(&key_up, std::mem::size_of::<INPUT>() as i32);
            if sent_u != 1 {
                return Err("Failed to send step key-up");
            }
        }
        sleep(delay);
    }

    // Send Shift up, Alt up
    let mods_up = [make_key_input(shift, true), make_key_input(alt, true)];
    unsafe {
        let sent = SendInput(&mods_up, std::mem::size_of::<INPUT>() as i32);
        if sent != mods_up.len() as u32 {
            return Err("Failed to send modifier key-up chord");
        }
    }
    sleep(delay);

    Ok(())
}

/// Deprecated: Steps from `current_cap` to `target_cap` using 5-FPS increments.
/// Replaced by direct memory write via `FramepacerIpc::set_target_fps`.
#[deprecated(
    note = "Keystroke injection disrupted gameplay controls. Use FramepacerIpc::set_target_fps instead."
)]
#[allow(deprecated)]
pub fn adjust_cap(current_cap: u32, target_cap: u32, settle_ms: u64) -> Result<i32, &'static str> {
    if current_cap == target_cap {
        return Ok(0);
    }
    let diff = target_cap as i64 - current_cap as i64;
    let steps = diff / 5;
    if steps > 0 {
        send_steps(VK_UP, steps as u32, settle_ms)?;
    } else {
        send_steps(VK_DOWN, steps.unsigned_abs() as u32, settle_ms)?;
    }
    Ok(steps as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_companion_hotkey_constants() {
        assert_eq!(COMPANION_HOTKEY_ID, 101);
        assert_eq!(COMPANION_HOTKEY_VK, 0x41);
    }

    #[test]
    fn test_make_key_input_has_valid_hardware_scancodes() {
        let keys = [
            VK_MENU,
            VK_SHIFT,
            VK_UP,
            VK_DOWN,
            VIRTUAL_KEY(0x4C), // 'L'
            VIRTUAL_KEY(0x41), // 'A'
        ];
        for &k in &keys {
            let inp_down = make_key_input(k, false);
            let inp_up = make_key_input(k, true);
            unsafe {
                let scan_down = inp_down.Anonymous.ki.wScan;
                let scan_up = inp_up.Anonymous.ki.wScan;
                assert!(scan_down > 0, "Scan code for {:?} must be non-zero", k);
                assert_eq!(scan_down, scan_up);
                assert!((inp_up.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP).0 != 0);
                assert!((inp_down.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP).0 == 0);
            }
        }
    }

    #[test]
    fn test_extended_key_flag_for_arrows() {
        let up_input = make_key_input(VK_UP, false);
        let down_input = make_key_input(VK_DOWN, false);
        let a_input = make_key_input(VIRTUAL_KEY(0x41), false);
        unsafe {
            assert!((up_input.Anonymous.ki.dwFlags & KEYEVENTF_EXTENDEDKEY).0 != 0);
            assert!((down_input.Anonymous.ki.dwFlags & KEYEVENTF_EXTENDEDKEY).0 != 0);
            assert!((a_input.Anonymous.ki.dwFlags & KEYEVENTF_EXTENDEDKEY).0 == 0);
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_adjust_cap_delta_calculation() {
        // Equal: 0 steps
        assert_eq!(adjust_cap(60, 60, 0).unwrap(), 0);
        // Step up: 50 -> 60 is +2 steps (+10 FPS)
        let diff: i64 = 60 - 50;
        assert_eq!(diff / 5, 2);
        // Step down: 200 -> 30 is -34 steps (-170 FPS)
        let diff_down: i64 = 30 - 200;
        assert_eq!(diff_down / 5, -34);
    }
}
