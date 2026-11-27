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

// --- Native FramePacer Protocol Offsets (verified against framepacer.exe & framepacer.dll) ---
/// Offset to FramePacer owner process ID (u32) in shared memory header
pub const OFFSET_OWNER_PID: usize = 0x00c; // 12 decimal

/// Offset to IPC active status flag (u32: 1 = active, 2 / 0xDEAD = shutting down / closed)
pub const OFFSET_IPC_ACTIVE: usize = 0x020; // 32 decimal

/// Offset to native target FPS in millihertz (u32: fps * 1000, e.g. 60000 = 60.0 FPS)
pub const OFFSET_REAL_TARGET_FPS_MHZ: usize = 0x028; // 40 decimal

/// Offset to native limiter enabled flag (u32: 1 = enabled, 0 = disabled)
pub const OFFSET_REAL_LIMITER_ENABLED: usize = 0x02c; // 44 decimal

/// Offset to frametime ring write index (u32) in FramePacer v5
pub const OFFSET_FRAMETIME_WRITE_IDX: usize = 0x030; // 48 decimal

/// Backward-compatible alias for offset 0x030 (frametime ring write index in FramePacer v5).
pub const OFFSET_REFRESH_RATE_MHZ: usize = OFFSET_FRAMETIME_WRITE_IDX;

/// Offset to process active state (u32: 2 = inactive/bypass, != 2 = active)
pub const OFFSET_PROCESS_ACTIVE_STATE: usize = 0x034; // 52 decimal

/// Offset to limiter mode (u32: 2 = normal/inactive)
pub const OFFSET_LIMITER_MODE: usize = OFFSET_PROCESS_ACTIVE_STATE;

/// Offset to the 120-entry `f64` frametime ring buffer (in ms) written by framepacer.dll
pub const OFFSET_FRAMETIME_RING: usize = 0x038; // 56 decimal

/// Number of `f64` entries in FramePacer's shared-memory frametime ring buffer
pub const FRAMETIME_RING_LEN: usize = 120;

/// Offset to in-game pacing DLL attachment status (u32: 1 = attached, 0 = detached)
pub const OFFSET_DLL_ATTACHED: usize = 0x408; // 1032 decimal

/// Offset to probe/state enum in FramePacer v5 (u32: NOT a PID — e.g. 2 during probe)
pub const OFFSET_GAME_PID: usize = 0x40c; // 1036 decimal

/// Offset to in-game target process ID written by in-game DLL hook (u32)
pub const OFFSET_DLL_GAME_PID: usize = 0x420; // 1056 decimal

/// Offset to in-game pacing active flag (u32: non-zero = actively pacing)
pub const OFFSET_PACING_ACTIVE: usize = 0x424; // 1060 decimal

/// Offset to IPC seqlock counter in FramePacer v5
pub const OFFSET_SEQLOCK: usize = 0x4dc; // 1244 decimal

// --- Backward-Compatible Offsets (Mock Test Harness Only) ---
/// Legacy specification offset for limiter_enabled (0x408).
/// In native FramePacer protocol, 0x408 is OFFSET_DLL_ATTACHED, and real limiter flag is 0x02c.
pub const OFFSET_LIMITER_ENABLED: usize = 0x408; // 1032 decimal

/// Specification compatibility offset for target_fps (0x414).
/// In native FramePacer protocol, 0x410..0x41F is a 16-byte API name buffer; NEVER write 0x414 on live FramePacer.
pub const OFFSET_TARGET_FPS: usize = 0x414; // 1044 decimal

#[derive(Debug)]
pub enum IpcError {
    NotConnected,
    SectionClosed,
    OpenMappingFailed(windows::core::Error),
    MapViewFailed(windows::core::Error),
    CreateMappingFailed(windows::core::Error),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => write!(f, "FramePacer IPC is not connected"),
            Self::SectionClosed => write!(f, "FramePacer IPC section was closed or shut down"),
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

    /// Verifies that the mapped FramePacer section has valid magic, version, size, and active == 1.
    pub fn is_alive(&self) -> bool {
        if !self.is_connected() {
            return false;
        }
        fence(Ordering::SeqCst);
        let alive = unsafe {
            let magic = std::ptr::read_volatile(self.view_ptr as *const u32);
            let version = std::ptr::read_volatile(self.view_ptr.add(4) as *const u32);
            let size = std::ptr::read_volatile(self.view_ptr.add(8) as *const u32);
            let active = std::ptr::read_volatile(self.view_ptr.add(OFFSET_IPC_ACTIVE) as *const u32);
            magic == IPC_MAGIC
                && version == IPC_VERSION
                && size == IPC_SECTION_SIZE as u32
                && active == 1
        };
        fence(Ordering::SeqCst);
        alive
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

        if !self.is_alive() {
            self.disconnect();
            return Err(IpcError::SectionClosed);
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
    /// Writes native millihertz at 0x028 (consumed by FramePacer DLL and EXE).
    /// Only mirrors direct FPS to 0x414 in mock sections (is_owner == true); on live FramePacer,
    /// 0x410..0x41F is an API name buffer that must never be overwritten.
    pub fn set_target_fps(&mut self, fps: u32) -> Result<(), IpcError> {
        self.ensure_connected()?;
        if !self.is_alive() {
            self.disconnect();
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        unsafe {
            // Native FramePacer protocol: target FPS in millihertz at offset 0x28
            let p_real = self.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *mut u32;
            std::ptr::write_volatile(p_real, fps.saturating_mul(1000));

            if self.is_owner {
                // Mirrored direct FPS at offset 0x414 (mock test harness only)
                let p_mirror = self.view_ptr.add(OFFSET_TARGET_FPS) as *mut u32;
                std::ptr::write_volatile(p_mirror, fps);
            }
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
        if !self.is_alive() {
            self.disconnect();
            return Err(IpcError::SectionClosed);
        }
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
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
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
                    real_fps_mhz.saturating_add(500) / 1000
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

    /// Returns whether the FramePacer in-game pacing DLL is currently attached (0x408 or 0x420 > 4).
    pub fn is_dll_attached(&self) -> Result<bool, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        let attached = unsafe {
            let attached_flag =
                std::ptr::read_volatile(self.view_ptr.add(OFFSET_DLL_ATTACHED) as *const u32);
            let dll_pid =
                std::ptr::read_volatile(self.view_ptr.add(OFFSET_DLL_GAME_PID) as *const u32);
            attached_flag != 0 || dll_pid > 4
        };
        fence(Ordering::SeqCst);
        Ok(attached)
    }

    /// Reads the FramePacer owner PID from `OFFSET_OWNER_PID` (`0x00C`) if valid (`> 4`).
    pub fn get_owner_pid(&self) -> Result<Option<u32>, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        let pid = unsafe {
            let p_owner_pid = self.view_ptr.add(OFFSET_OWNER_PID) as *const u32;
            let owner_pid = std::ptr::read_volatile(p_owner_pid);
            if owner_pid > 4 { Some(owner_pid) } else { None }
        };
        fence(Ordering::SeqCst);
        Ok(pid)
    }

    /// Reads the target game PID from `OFFSET_DLL_GAME_PID` (`0x420`) if valid (`> 4`).
    /// Does NOT fall back to `0x40C`, which holds a small probe state enum (e.g. `2`), not a PID.
    pub fn get_attached_pid(&self) -> Result<Option<u32>, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        let pid = unsafe {
            let p_dll_pid = self.view_ptr.add(OFFSET_DLL_GAME_PID) as *const u32;
            let dll_pid = std::ptr::read_volatile(p_dll_pid);
            if dll_pid > 4 { Some(dll_pid) } else { None }
        };
        fence(Ordering::SeqCst);
        Ok(pid)
    }

    /// Reads up to `max_samples` recent frametime intervals (in ms) from FramePacer's
    /// 120-entry `f64` ring buffer (`0x038`) using seqlock `0x4DC`, returning `(write_idx, samples)`.
    pub fn read_frametime_ring(&self, max_samples: usize) -> Result<(u32, Vec<f64>), IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }

        for _ in 0..8 {
            let (seq1, write_idx, raw_samples, seq2) = unsafe {
                let seq1 = std::ptr::read_volatile(self.view_ptr.add(OFFSET_SEQLOCK) as *const u32);
                if (seq1 & 1) != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                fence(Ordering::SeqCst);

                let write_idx = std::ptr::read_volatile(
                    self.view_ptr.add(OFFSET_FRAMETIME_WRITE_IDX) as *const u32,
                );
                let count = max_samples
                    .min(FRAMETIME_RING_LEN)
                    .min(write_idx as usize);
                let mut buf = Vec::with_capacity(count);
                let start = write_idx.saturating_sub(count as u32);
                for i in 0..count {
                    let idx = start.wrapping_add(i as u32) as usize;
                    let slot = idx % FRAMETIME_RING_LEN;
                    let byte_off = OFFSET_FRAMETIME_RING + slot * std::mem::size_of::<f64>();
                    let p = self.view_ptr.add(byte_off) as *const f64;
                    let val = std::ptr::read_unaligned(p);
                    if val.is_finite() && val > 0.5 && val < 1000.0 {
                        buf.push(val);
                    }
                }

                fence(Ordering::SeqCst);
                let seq2 = std::ptr::read_volatile(self.view_ptr.add(OFFSET_SEQLOCK) as *const u32);
                (seq1, write_idx, buf, seq2)
            };

            if seq1 == seq2 {
                return Ok((write_idx, raw_samples));
            }
        }

        Ok((0, Vec::new()))
    }

    /// Returns whether FramePacer reports the target process as active (offset 0x034 != 2).
    pub fn is_process_active(&self) -> Result<bool, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        let active = unsafe {
            std::ptr::read_volatile(self.view_ptr.add(OFFSET_PROCESS_ACTIVE_STATE) as *const u32)
                != 2
        };
        fence(Ordering::SeqCst);
        Ok(active)
    }

    /// Returns whether the in-game DLL reports pacing is currently active (offset 0x424 != 0).
    pub fn is_pacing_active(&self) -> Result<bool, IpcError> {
        if !self.is_connected() {
            return Err(IpcError::NotConnected);
        }
        if !self.is_alive() {
            return Err(IpcError::SectionClosed);
        }
        fence(Ordering::SeqCst);
        let pacing = unsafe {
            std::ptr::read_volatile(self.view_ptr.add(OFFSET_PACING_ACTIVE) as *const u32) != 0
        };
        fence(Ordering::SeqCst);
        Ok(pacing)
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
        assert!(!ipc.is_alive());
        assert!(ipc.get_state().is_err());
    }

    #[test]
    fn test_client_set_target_fps_does_not_overwrite_0x414() {
        let mock_name = format!(r"Local\TestPreserve0x414_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        let api_buf: [u8; 16] = *b"DXGI\0\0\0\0D3D12\0\0\0";
        unsafe {
            std::ptr::copy_nonoverlapping(api_buf.as_ptr(), server.view_ptr.add(0x410), 16);
        }

        let mut client = FramepacerIpc::with_name(&mock_name);
        client.connect().expect("Client failed to connect");
        client.set_target_fps(90).expect("set_target_fps failed");

        unsafe {
            let mhz = std::ptr::read_volatile(
                server.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *const u32
            );
            assert_eq!(mhz, 90_000);
            let actual = std::slice::from_raw_parts(server.view_ptr.add(0x410), 16);
            assert_eq!(actual, &api_buf, "Client must never clobber 0x410..0x41F");
        }
    }

    #[test]
    fn test_dead_section_0xdead_detection() {
        let mock_name = format!(r"Local\TestDeadSection_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        let mut client = FramepacerIpc::with_name(&mock_name);
        client.connect().expect("Client failed to connect");
        assert!(client.is_alive());

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_IPC_ACTIVE) as *mut u32, 0xDEAD);
        }

        assert!(!client.is_alive());
        assert!(matches!(client.get_state(), Err(IpcError::SectionClosed)));
    }

    #[test]
    fn test_millihertz_rounding() {
        let mock_name = format!(r"Local\TestMhzRounding_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        unsafe {
            std::ptr::write_volatile(
                server.view_ptr.add(OFFSET_REAL_TARGET_FPS_MHZ) as *mut u32,
                59_940,
            );
        }
        assert_eq!(server.get_target_fps().unwrap(), 60);
    }

    #[test]
    fn test_get_attached_pid_ignores_0x40c_state_enum() {
        let mock_name = format!(r"Local\TestIgnore0x40c_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_DLL_ATTACHED) as *mut u32, 1);
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_GAME_PID) as *mut u32, 2);
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_DLL_GAME_PID) as *mut u32, 0);
        }
        assert_eq!(server.get_attached_pid().unwrap(), None);

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_DLL_GAME_PID) as *mut u32, 9828);
        }
        assert_eq!(server.get_attached_pid().unwrap(), Some(9828));
    }

    #[test]
    fn test_read_frametime_ring() {
        let mock_name = format!(r"Local\TestFrametimeRing_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        unsafe {
            for i in 0..10usize {
                let p = server
                    .view_ptr
                    .add(OFFSET_FRAMETIME_RING + i * std::mem::size_of::<f64>())
                    as *mut f64;
                std::ptr::write_unaligned(p, 16.667);
            }
            std::ptr::write_volatile(
                server.view_ptr.add(OFFSET_FRAMETIME_WRITE_IDX) as *mut u32,
                10,
            );
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_SEQLOCK) as *mut u32, 20);
        }

        let (widx, samples) = server.read_frametime_ring(10).unwrap();
        assert_eq!(widx, 10);
        assert_eq!(samples.len(), 10);
        for s in samples {
            assert!((s - 16.667).abs() < 0.001);
        }
    }

    #[test]
    fn test_get_owner_pid() {
        let mock_name = format!(r"Local\TestOwnerPid_{}", std::process::id());
        let server = FramepacerIpc::create_mock_for_test(&mock_name)
            .expect("Failed to create mock shared memory section");

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_OWNER_PID) as *mut u32, 0);
        }
        assert_eq!(server.get_owner_pid().unwrap(), None);

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_OWNER_PID) as *mut u32, 4);
        }
        assert_eq!(server.get_owner_pid().unwrap(), None);

        unsafe {
            std::ptr::write_volatile(server.view_ptr.add(OFFSET_OWNER_PID) as *mut u32, 9468);
        }
        assert_eq!(server.get_owner_pid().unwrap(), Some(9468));
    }
}
