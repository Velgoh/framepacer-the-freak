use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_MENU, VK_SHIFT,
};

pub const COMPANION_HOTKEY_VK: u32 = 0x41; // 'A'

/// Checks whether a key is currently physically held down.
pub fn is_key_physically_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

/// Checks whether the companion toggle hotkey (Alt + Shift + A) was just pressed.
/// Performs edge detection against `was_down` so holding the keys does not trigger repeatedly.
pub fn check_toggle_hotkey(was_down: &mut bool) -> bool {
    let alt_down = is_key_physically_down(VK_MENU);
    let shift_down = is_key_physically_down(VK_SHIFT);
    let a_down = unsafe {
        let key_state = GetAsyncKeyState(COMPANION_HOTKEY_VK as i32) as u16;
        (key_state & 0x8001) != 0
    };

    let is_down = alt_down && shift_down && a_down;
    let just_pressed = is_down && !*was_down;
    *was_down = is_down;
    just_pressed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_companion_hotkey_constants() {
        assert_eq!(COMPANION_HOTKEY_VK, 0x41);
    }

    #[test]
    fn test_edge_detection_logic() {
        let mut was_down = false;
        // In unit test environment, keys are not pressed
        let triggered = check_toggle_hotkey(&mut was_down);
        assert!(!triggered);
        assert!(!was_down);
    }
}
