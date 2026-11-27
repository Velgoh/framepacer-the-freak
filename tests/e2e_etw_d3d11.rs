use framepacer_the_freak::etw::{EtwTraceSession, get_global_recorder};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[test]
fn test_realtime_etw_dxgi_present_capture() {
    let recorder = get_global_recorder();

    // Stop any leftover sessions that might hold exclusive provider handles
    let _ = std::process::Command::new("logman")
        .args(["stop", "FramePacerCompanionETW", "-ets"])
        .output();
    let _ = std::process::Command::new("logman")
        .args(["stop", "FramePacerTheFreakETW", "-ets"])
        .output();
    let _ = std::process::Command::new("logman")
        .args(["stop", "FPAutoE2ETestTrace", "-ets"])
        .output();

    // Start ETW trace session with DXGI provider enabled at Verbose level 5
    let session = match EtwTraceSession::start("FPAutoE2ETestTrace") {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[SKIP] Could not start ETW session in test environment: {}",
                e
            );
            return;
        }
    };

    // Locate release or debug binary of fp-testgame based on build configuration
    let debug_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("fp-testgame.exe");
    let release_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("fp-testgame.exe");
    let exe = if cfg!(debug_assertions) && debug_path.exists() {
        debug_path
    } else if release_path.exists() {
        release_path
    } else {
        debug_path
    };

    let mut child = Command::new(&exe)
        .args(["--duration", "2.0", "--fps", "60"])
        .spawn()
        .expect("Failed to spawn fp-testgame");

    let child_pid = child.id();
    let status = child.wait().expect("Failed to wait on fp-testgame");
    assert!(status.success());

    // Flush kernel ETW buffers into realtime consumer
    session.flush();
    std::thread::sleep(Duration::from_millis(500));

    // Verify frames recorded for fp-testgame PID
    let (frame_count, measured_fps) = {
        let rec = recorder.lock().unwrap();
        let intervals = rec.get_frame_intervals(child_pid, Duration::from_secs(5));
        let fps = rec.get_current_fps(child_pid, Duration::from_secs(5));
        (intervals.len(), fps)
    };

    println!(
        "[E2E ETW] fp-testgame (PID {}) captured frames: {}, measured FPS: {:?}",
        child_pid, frame_count, measured_fps
    );

    assert!(
        frame_count >= 50,
        "Expected at least 50 frames captured by ETW over 2s render, got {}",
        frame_count
    );
    let fps = measured_fps.expect("Expected valid FPS measurement");
    assert!(
        (fps - 60.0).abs() < 5.0,
        "Measured FPS {:.2} outside expected 60 FPS range",
        fps
    );

    // Stop trace session cleanly
    drop(session);
}
