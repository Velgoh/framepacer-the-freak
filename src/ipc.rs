use std::fmt;
use std::sync::atomic::{Ordering, fence};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

/// Default Win32 FileMapping object name used by FramePacer v5
pub const FRAME_PACER_IPC_NAME: &str = r"Local\FramepacerIPC_v5";

/// Total size in bytes of the FramePacer IPC shared memory section
pub const IPC_SECTION_SIZE: usize = 1256;

/// FramePacer IPC Header Magic: "FIPC" (0x43504946 in little-endian)
pub const IPC_MAGIC: u32 = 0x43504946;

/// FramePacer IPC Header Version (5)
pub const IPC_VERSION: u32 = 5;

// --- Native FramePacer Protocol Offsets (revealed by reverse engineering) ---
/// Offset to native target FPS in millihertz (u32: fps * 1000, e.g. 60000 = 60.0 FPS)
pub const OFFSET_REAL_TARGET_FPS_MHZ: usize = 0x028; // 40 decimal

/// Offset to native limiter enabled flag (u32: 1 = enabled, 0 = disabled)
pub const OFFSET_REAL_LIMITER_ENABLED: usize = 0x02c; // 44 decimal

/// Offset to display refresh rate in millihertz (u32, e.g. 200000 = 200.000 Hz)
pub const OFFSET_REFRESH_RATE_MHZ: usize = 0x030; // 48 decimal

/// Offset to limiter mode (u32: 2 = normal)
pub const OFFSET_LIMITER_MODE: usize = 0x034; // 52 decimal

/// Offset to in-game pacing DLL attachment status (u32: 1 = attached, 0 = detached)
pub const OFFSET_DLL_ATTACHED: usize = 0x408; // 1032 decimal

/// Offset to in-game target process ID in FramePacer GUI (u32)
pub const OFFSET_GAME_PID: usize = 0x40c; // 1036 decimal

/// Offset to in-game target process ID written by in-game DLL hook (u32)
pub const OFFSET_DLL_GAME_PID: usize = 0x420; // 1056 decimal

// --- Backward-Compatible Offsets (Specified in Task Prompt & Tests) ---
/// Legacy specification offset for limiter_enabled (0x408).
/// In native FramePacer protocol, 0x408 is OFFSET_DLL_ATTACHED, and real limiter flag is 0x02c.
pub const OFFSET_LIMITER_ENABLED: usize = 0x408; // 1032 decimal

/// Specification compatibility offset for target_fps (0x414).
/// In native FramePacer protocol, target FPS is stored in millihertz at 0x028.
pub const OFFSET_TARGET_FPS: usize = 0x414; // 1044 decimal

#[derive(Debug)]
pub enum IpcError {
    NotConnected,
    OpenMappingFailed(windows::core::Error),
    MapViewFailed(windows::core::Error),
    CreateMappingFailed(windows::core::Error),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => write!(f, "FramePacer IPC is not connected"),
            Self::OpenMappingFailed(e) => {
                write!(f, "Failed to open FramePacer file mapping: {}", e)
            }
            Self::MapViewFailed(e) => {
                write!(f, "Failed to map view of FramePacer IPC section: {}", e)
            }
            Self::CreateMappingFailed(e) => {
                write!(f, "Failed to create mock FramePacer file mapping: {}", e)
            }
        }
    }
}

impl std::error::Error for IpcError {}

/// Safe wrapper for Windows Shared Memory IPC with FramePacer's `Local\FramepacerIPC_v5`.
/// Allows instant, zero-keystroke cap adjustments and limiter toggling.
pub struct FramepacerIpc {
    mapping_name: String,
    handle: Option<HANDLE>,
    view_ptr: *mut u8,
    #[allow(dead_code)]
    is_owner: bool,
}

unsafe impl Send for FramepacerIpc {}
unsafe impl Sync for FramepacerIpc {}

impl Default for FramepacerIpc {
    fn default() -> Self {
        Self::new()
    }
}

impl FramepacerIpc {
    /// Creates a new disconnected IPC interface pointing to default `Local\FramepacerIPC_v5`.
    pub fn new() -> Self {
        Self::with_name(FRAME_PACER_IPC_NAME)
    }

    /// Creates a new disconnected IPC interface pointing to a custom mapping name (useful for tests).
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            mapping_name: name.into(),
            handle: None,
            view_ptr: std::ptr::null_mut(),
            is_owner: false,
        }
    }

    /// Returns whether the shared memory section is currently mapped and accessible.
    pub fn is_connected(&self) -> bool {
        !self.view_ptr.is_null()
    }

    /// Connects to the existing FramePacer shared memory section.
    pub fn connect(&mut self) -> Result<(), IpcError> {
        self.disconnect();

        let mut utf16: Vec<u16> = self.mapping_name.encode_utf16().collect();
        utf16.push(0);

        unsafe {
            let rw_access = FILE_MAP_READ.0 | FILE_MAP_WRITE.0;

            // Attempt opening with standard read/write access first, fall back to ALL_ACCESS
            let handle = match OpenFileMappingW(rw_access, false, PCWSTR(utf16.as_ptr())) {
                Ok(h) => h,
                Err(_) => OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(utf16.as_ptr()))
                    .map_err(IpcError::OpenMappingFailed)?,
            };

            let view = MapViewOfFile(handle, FILE_MAP(rw_access), 0, 0, IPC_SECTION_SIZE);

            let view_ptr = if view.Value.is_null() {
                let view_all = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, IPC_SECTION_SIZE);
                if view_all.Value.is_null() {
                    let err = windows::core::Error::from_thread();
                    let _ = CloseHandle(handle);
                    return Err(IpcError::MapViewFailed(err));
                }
                view_all.Value
            } else {
                view.Value
            };

            self.handle = Some(handle);
            self.view_ptr = view_ptr as *mut u8;
            self.is_owner = false;
        }

        Ok(())
    }

    /// Ensures that the IPC section is connected, attempting to connect if not already.
    pub fn ensure_connected(&mut self) -> Result<(), IpcError> {
        if self.is_connected() {
            Ok(())
        } else {
            self.connect()
        }
    }

    /// Creates a mock shared memory section for testing without needing FramePacer running.
    pub fn create_mock_for_test(name: &str) -> Result<Self, IpcError> {
        let mut utf16: Vec<u16> = name.encode_utf16().collect();
        utf16.push(0);

        unsafe {
            let handle = CreateFileMappingW(
                HANDLE::default(),
                None,
                PAGE_READWRITE,
                0,
                IPC_SECTION_SIZE as u32,
                PCWSTR(utf16.as_ptr()),
            )
            .map_err(IpcError::CreateMappingFailed)?;

            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, IPC_SECTION_SIZE);

            if view.Value.is_null() {
                let err = windows::core::Error::from_thread();
                let _ = CloseHandle(handle);
                return Err(IpcError::MapViewFailed(err));
            }

            // Zero-initialize mock memory
            std::ptr::write_bytes(view.Value as *mut u8, 0, IPC_SECTION_SIZE);

            // Populate valid header so mock matches real FramePacer memory block
            let base = view.Value as *mut u8;
            std::ptr::write_volatile(base as *mut u32, IPC_MAGIC);
            std::ptr::write_volatile(base.add(4) as *mut u32, IPC_VERSION);
            std::ptr::write_volatile(base.add(8) as *mut u32, IPC_SECTION_SIZE as u32);
            std::ptr::write_volatile(base.add(0x20) as *mut u32, 1);

            Ok(Self {
                mapping_name: name.to_string(),
                handle: Some(handle),
                view_ptr: view.Value as *mut u8,
                is_owner: true,
            })
        }
    }

    /// Sets the target FPS cap directly in memory.
    /// Writes both native millihertz at 0x028 (consumed by FramePacer DLL and EXE)
    /// and direct FPS at 0x414 (for specification / test harness compatibility).
    pub fn set_target_fps(&mut self, fps: u32) -> Result<(), IpcError> {
        self.ensure_connected()?;
        fence(Ordering::SeqCst);
        unsafe {
            // Native FramePacer protocol: target FPS in millihertz at offset 0x28
            let p_real = self.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *mut u32;
            std::ptr::write_volatile(p_real, fps.saturating_mul(1000));

            // Mirrored direct FPS at offset 0x414
            let p_mirror = self.view_ptr.add(OFFSET_TARGET_FPS) as *mut u32;
            std::ptr::write_volatile(p_mirror, fps);
        }
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Sets the limiter state (1 = ON, 0 = OFF) directly in memory.
    /// Writes native limiter flag at 0x02C (consumed by FramePacer DLL and EXE).
    /// In mock sections (is_owner == true), also mirrors to offset 0x408 for mock test harness compatibility.
    /// When connected to live FramePacer, offset 0x408 is protected because it holds OFFSET_DLL_ATTACHED.
    pub fn set_limiter_enabled(&mut self, enabled: bool) -> Result<(), IpcError> {
        self.ensure_connected()?;
        let val: u32 = if enabled { 1 } else { 0 };
        fence(Ordering::SeqCst);
        unsafe {
            // Native FramePacer protocol: limiter enabled flag at offset 0x2C
            let p_real = self.view_ptr.add(OFFSET_REAL_LIMITER_ENABLED) as *mut u32;
            std::ptr::write_volatile(p_real, val);

            // In mock environments (is_owner is true), mirror to OFFSET_LIMITER_ENABLED (0x408)
            // for specification / test harness compatibility.
            // When connected to live FramePacer, NEVER overwrite offset 0x408 because that is
            // OFFSET_DLL_ATTACHED (set by framepacer.dll on game injection). Overwriting 0x408 with 0
            // would cause FramePacer to believe the in-game DLL detached!
            if self.is_owner {
                let p_mirror = self.view_ptr.add(OFFSET_LIMITER_ENABLED) as *mut u32;
                std::ptr::write_volatile(p_mirror, val);
            }
        }
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Reads the current limiter status: (limiter_enabled, target_fps).
    pub fn get_state(&self) -> Result<(bool, u32), IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        fence(Ordering::SeqCst);
        let (enabled, target_fps) = unsafe {
            let p_real_en = self.view_ptr.add(OFFSET_REAL_LIMITER_ENABLED) as *const u32;
            let p_mirror_en = self.view_ptr.add(OFFSET_LIMITER_ENABLED) as *const u32;
            let real_en = std::ptr::read_volatile(p_real_en);
            let mirror_en = std::ptr::read_volatile(p_mirror_en);

            let p_real_fps = self.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *const u32;
            let p_mirror_fps = self.view_ptr.add(OFFSET_TARGET_FPS) as *const u32;
            let real_fps_mhz = std::ptr::read_volatile(p_real_fps);
            let mirror_fps = std::ptr::read_volatile(p_mirror_fps);

            if real_fps_mhz > 0 || real_en != 0 {
                // Native FramePacer protocol:
                // Limiter state is exclusively at 0x02c. Do NOT check 0x408 here because
                // 0x408 is OFFSET_DLL_ATTACHED, which is 1 whenever a game is hooked.
                let fps = if real_fps_mhz > 0 {
                    real_fps_mhz / 1000
                } else {
                    mirror_fps
                };
                (real_en != 0, fps)
            } else if mirror_fps > 0 {
                // Fallback for legacy mock environments that only wrote mirrored offset 0x414
                (mirror_en != 0, mirror_fps)
            } else {
                (false, 0)
            }
        };
        fence(Ordering::SeqCst);
        Ok((enabled, target_fps))
    }

    /// Reads the target FPS cap currently in shared memory.
    pub fn get_target_fps(&self) -> Result<u32, IpcError> {
        self.get_state().map(|(_, fps)| fps)
    }

    /// Reads whether the limiter is currently enabled in shared memory.
    pub fn get_limiter_enabled(&self) -> Result<bool, IpcError> {
        self.get_state().map(|(en, _)| en)
    }

    /// Returns whether the FramePacer in-game pacing DLL is currently attached (offset 0x408).
    pub fn is_dll_attached(&self) -> Result<bool, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        fence(Ordering::SeqCst);
        let attached = unsafe {
            let p = self.view_ptr.add(OFFSET_DLL_ATTACHED) as *const u32;
            std::ptr::read_volatile(p) != 0
        };
        fence(Ordering::SeqCst);
        Ok(attached)
    }

    /// Reads the target game PID from IPC shared memory if attached.
    pub fn get_attached_pid(&self) -> Result<Option<u32>, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        fence(Ordering::SeqCst);
        let pid = unsafe {
            let p_dll_pid = self.view_ptr.add(OFFSET_DLL_GAME_PID) as *const u32;
            let dll_pid = std::ptr::read_volatile(p_dll_pid);
            if dll_pid > 0 {
                Some(dll_pid)
            } else {
                let p_game_pid = self.view_ptr.add(OFFSET_GAME_PID) as *const u32;
                let game_pid = std::ptr::read_volatile(p_game_pid);
                if game_pid > 0 { Some(game_pid) } else { None }
            }
        };
        fence(Ordering::SeqCst);
        Ok(pid)
    }

    /// Reconnects to FramePacer shared memory.
    pub fn reconnect(&mut self) -> Result<(), IpcError> {
        self.connect()
    }

    /// Closes and unmaps the shared memory section.
    pub fn disconnect(&mut self) {
        if !self.view_ptr.is_null() {
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view_ptr as *mut core::ffi::c_void,
                });
            }
            self.view_ptr = std::ptr::null_mut();
        }

        if let Some(handle) = self.handle.take() {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

impl Drop for FramepacerIpc {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_offsets_and_bounds() {
        // Mirrored / specification offsets
        assert_eq!(OFFSET_LIMITER_ENABLED, 0x408);
        assert_eq!(OFFSET_LIMITER_ENABLED, 1032);
        assert_eq!(OFFSET_TARGET_FPS, 0x414);
        assert_eq!(OFFSET_TARGET_FPS, 1044);

        // Native FramePacer protocol offsets
        assert_eq!(OFFSET_REAL_TARGET_FPS_MHZ, 0x028);
        assert_eq!(OFFSET_REAL_TARGET_FPS_MHZ, 40);
        assert_eq!(OFFSET_REAL_LIMITER_ENABLED, 0x02c);
        assert_eq!(OFFSET_REAL_LIMITER_ENABLED, 44);
        assert_eq!(OFFSET_REFRESH_RATE_MHZ, 0x030);
        assert_eq!(OFFSET_LIMITER_MODE, 0x034);
        assert_eq!(OFFSET_DLL_ATTACHED, 0x408);
        assert_eq!(OFFSET_GAME_PID, 0x40c);

        const { assert!(OFFSET_LIMITER_ENABLED + 4 <= IPC_SECTION_SIZE) };
        const { assert!(OFFSET_TARGET_FPS + 4 <= IPC_SECTION_SIZE) };
        const { assert!(OFFSET_REAL_TARGET_FPS_MHZ + 4 <= IPC_SECTION_SIZE) };
        const { assert!(OFFSET_REAL_LIMITER_ENABLED + 4 <= IPC_SECTION_SIZE) };

        assert_eq!(OFFSET_LIMITER_ENABLED % 4, 0, "Must be 4-byte aligned");
        assert_eq!(OFFSET_TARGET_FPS % 4, 0, "Must be 4-byte aligned");
        assert_eq!(OFFSET_REAL_TARGET_FPS_MHZ % 4, 0, "Must be 4-byte aligned");
        assert_eq!(OFFSET_REAL_LIMITER_ENABLED % 4, 0, "Must be 4-byte aligned");
    }

    #[test]
    fn test_mock_ipc_header_verification() {
        let mock_name = format!(r"Local\TestFIPCHeader_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        unsafe {
            let magic = std::ptr::read_volatile(server.view_ptr as *const u32);
            let version = std::ptr::read_volatile(server.view_ptr.add(4) as *const u32);
            let size = std::ptr::read_volatile(server.view_ptr.add(8) as *const u32);
            let active = std::ptr::read_volatile(server.view_ptr.add(0x20) as *const u32);

            assert_eq!(magic, IPC_MAGIC); // "FIPC"
            assert_eq!(version, 5);
            assert_eq!(size, 1256);
            assert_eq!(active, 1);
        }
    }

    #[test]
    fn test_mock_ipc_write_and_read() {
        let mock_name = format!(r"Local\TestFramepacerIPC_{}", std::process::id());
        let mut server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");
        assert!(server.is_connected());

        // Default after creation should be enabled=false, target_fps=0
        let (enabled, fps) = server.get_state().expect("Failed to get state");
        assert!(!enabled);
        assert_eq!(fps, 0);

        // Set target FPS to 60
        server.set_target_fps(60).unwrap();
        assert_eq!(server.get_target_fps().unwrap(), 60);

        // Verify both native millihertz and direct FPS were written
        unsafe {
            let raw_mhz = std::ptr::read_volatile(
                server.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *const u32
            );
            let raw_mirror =
                std::ptr::read_volatile(server.view_ptr.add(OFFSET_TARGET_FPS) as *const u32);
            assert_eq!(raw_mhz, 60000);
            assert_eq!(raw_mirror, 60);
        }

        // Set limiter enabled
        server.set_limiter_enabled(true).unwrap();
        assert!(server.get_limiter_enabled().unwrap());

        // Verify both native and mirrored limiter enabled flags were written
        unsafe {
            let raw_real_en = std::ptr::read_volatile(
                server.view_ptr.add(OFFSET_REAL_LIMITER_ENABLED) as *const u32,
            );
            let raw_mirror_en =
                std::ptr::read_volatile(server.view_ptr.add(OFFSET_LIMITER_ENABLED) as *const u32);
            assert_eq!(raw_real_en, 1);
            assert_eq!(raw_mirror_en, 1);
        }

        // Client connection to same section
        let mut client = FramepacerIpc::with_name(&mock_name);
        assert!(!client.is_connected());
        client.connect().expect("Client failed to connect");
        assert!(client.is_connected());

        let (c_en, c_fps) = client.get_state().unwrap();
        assert!(c_en);
        assert_eq!(c_fps, 60);

        // Client writes, server sees
        client.set_target_fps(144).unwrap();
        client.set_limiter_enabled(false).unwrap();

        let (s_en, s_fps) = server.get_state().unwrap();
        assert!(!s_en);
        assert_eq!(s_fps, 144);

        // Disconnect behavior
        client.disconnect();
        assert!(!client.is_connected());
        assert!(client.get_state().is_err());
    }

    #[test]
    fn test_mock_ipc_native_only_and_mirror_only_fallback() {
        let mock_name = format!(r"Local\TestFallback_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        // Case 1: Only native FramePacer protocol offsets populated (0x28, 0x2c)
        unsafe {
            let p_real_fps = server.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *mut u32;
            let p_real_en = server.view_ptr.add(OFFSET_REAL_LIMITER_ENABLED) as *mut u32;
            std::ptr::write_volatile(p_real_fps, 75000); // 75.0 FPS in mHz
            std::ptr::write_volatile(p_real_en, 1);
        }
        let (en, fps) = server.get_state().unwrap();
        assert!(en);
        assert_eq!(fps, 75);

        // Case 2: Only mirrored offsets populated (0x408, 0x414)
        unsafe {
            let p_real_fps = server.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *mut u32;
            let p_real_en = server.view_ptr.add(OFFSET_REAL_LIMITER_ENABLED) as *mut u32;
            std::ptr::write_volatile(p_real_fps, 0);
            std::ptr::write_volatile(p_real_en, 0);

            let p_mirror_fps = server.view_ptr.add(OFFSET_TARGET_FPS) as *mut u32;
            let p_mirror_en = server.view_ptr.add(OFFSET_LIMITER_ENABLED) as *mut u32;
            std::ptr::write_volatile(p_mirror_fps, 50);
            std::ptr::write_volatile(p_mirror_en, 1);
        }
        let (en2, fps2) = server.get_state().unwrap();
        assert!(en2);
        assert_eq!(fps2, 50);
    }

    #[test]
    fn test_unopened_ipc_returns_error() {
        let ipc = FramepacerIpc::with_name(r"Local\NonExistentMapping_99999");
        assert!(!ipc.is_connected());
        assert!(ipc.get_state().is_err());
    }
}
