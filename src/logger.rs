use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger {
    file: File,
}

static GLOBAL_LOGGER: OnceLock<Mutex<FileLogger>> = OnceLock::new();

/// Initializes the global file logger at `path`, rotating to `.old` if it exceeds 2 MB.
pub fn init_logger(path: PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_LOG_BYTES
    {
        let mut old_path = path.clone();
        old_path.set_extension("log.old");
        let _ = std::fs::remove_file(&old_path);
        let _ = std::fs::rename(&path, &old_path);
    }

    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = GLOBAL_LOGGER.set(Mutex::new(FileLogger { file }));
    }
}

fn format_utc_timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant civil-from-days algorithm (UTC)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Writes a structured line to the log file (flushing immediately) and mirrors to stdout/stderr.
pub fn log_line(level: &str, msg: &str) {
    let ts = format_utc_timestamp();
    let line = format!("{} | {} | {}", ts, level, msg);

    if level == "ERROR" || level == "WARN" {
        eprintln!("[{}] {}", level, msg);
    } else {
        println!("[{}] {}", level, msg);
    }

    if let Some(lock) = GLOBAL_LOGGER.get()
        && let Ok(mut logger) = lock.lock()
    {
        let _ = writeln!(logger.file, "{}", line);
        let _ = logger.file.flush();
    }
}

pub fn log_info(msg: impl AsRef<str>) {
    log_line("INFO", msg.as_ref());
}

pub fn log_warn(msg: impl AsRef<str>) {
    log_line("WARN", msg.as_ref());
}

pub fn log_error(msg: impl AsRef<str>) {
    log_line("ERROR", msg.as_ref());
}

/// Returns whether the current process is running with elevated (Administrator) privileges.
pub fn is_process_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utc_timestamp_format() {
        let ts = format_utc_timestamp();
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn test_logger_writes_and_flushes() {
        let dir = std::env::temp_dir().join("framepacer_logger_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_freak.log");
        let _ = std::fs::remove_file(&path);

        // Write directly via FileLogger check if GLOBAL_LOGGER is already set
        init_logger(path.clone());
        log_info("logger_test_entry");

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(content.contains("logger_test_entry"));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
    }
}
