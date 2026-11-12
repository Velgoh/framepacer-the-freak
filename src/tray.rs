use windows::Win32::Foundation::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const WM_TRAY_ICON: u32 = WM_USER + 101;
pub const TRAY_HOTKEY_ID: i32 = 1001;

pub const ID_TRAY_TOGGLE: usize = 2001;
pub const ID_TRAY_CONFIG: usize = 2002;
pub const ID_TRAY_EXIT: usize = 2003;

pub struct TrayIcon {
    pub hwnd: HWND,
    nid: NOTIFYICONDATAW,
}

unsafe impl Send for TrayIcon {}

impl TrayIcon {
    pub fn create(hwnd: HWND, initial_tip: &str) -> Result<Self> {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY_ICON,
            ..Default::default()
        };

        unsafe {
            nid.hIcon = LoadIconW(None, IDI_APPLICATION)?;
            copy_str_to_wide_buf(initial_tip, &mut nid.szTip);
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }

        Ok(Self { hwnd, nid })
    }

    pub fn update_tip(&mut self, tip: &str) {
        copy_str_to_wide_buf(tip, &mut self.nid.szTip);
        self.nid.uFlags = NIF_TIP;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
        }
    }
}

fn copy_str_to_wide_buf(s: &str, buf: &mut [u16]) {
    let mut i = 0;
    for c in s.encode_utf16() {
        if i + 1 >= buf.len() {
            break;
        }
        buf[i] = c;
        i += 1;
    }
    buf[i] = 0;
}

pub fn show_tray_menu(hwnd: HWND, is_auto_enabled: bool) {
    unsafe {
        let hmenu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };

        let toggle_text = if is_auto_enabled {
            w!("Disable Auto (Alt+Shift+A)")
        } else {
            w!("Enable Auto (Alt+Shift+A)")
        };

        let mut check_flag = MENU_ITEM_FLAGS(0);
        if is_auto_enabled {
            check_flag |= MF_CHECKED;
        }

        let _ = AppendMenuW(hmenu, check_flag | MF_STRING, ID_TRAY_TOGGLE, toggle_text);
        let _ = AppendMenuW(hmenu, MENU_ITEM_FLAGS(0) | MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            hmenu,
            MENU_ITEM_FLAGS(0) | MF_STRING,
            ID_TRAY_CONFIG,
            w!("Open Config (config.toml)"),
        );
        let _ = AppendMenuW(hmenu, MENU_ITEM_FLAGS(0) | MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            hmenu,
            MENU_ITEM_FLAGS(0) | MF_STRING,
            ID_TRAY_EXIT,
            w!("Exit FramePacer Auto"),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(hmenu, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);
    }
}
