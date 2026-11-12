use framepacer_the_freak::etw::EtwTraceSession;
use framepacer_the_freak::ipc::FramepacerIpc;
use framepacer_the_freak::logtail::LogTailer;
use std::path::PathBuf;

fn main() {
    println!("=== FramePacer Auto Feasibility & Health Probe ===");

    // 1. Check FramePacer log accessibility
    let log_path =
        PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\framepacer\framepacer.log");
    println!("Testing log file access at {:?}...", log_path);
    let mut tailer = LogTailer::new(&log_path, true);
    println!("Log tailer initialized successfully.");
    for ev in tailer.poll_log_events() {
        println!("  [LOG] {} | {:?}", ev.event, ev.fields);
    }

    // 2. Test ETW session start & flush
    println!("Testing ETW Realtime Session startup...");
    match EtwTraceSession::start("FramePacerProbeTrace") {
        Ok(session) => {
            println!(
                "  [OK] ETW session started with DXGI and D3D9 providers enabled (Verbose level 5)."
            );
            session.flush();
            println!("  [OK] ETW session buffer flush succeeded.");
            drop(session);
            println!("  [OK] ETW session stopped and cleaned up cleanly.");
        }
        Err(e) => {
            println!("  [WARN] ETW session error: {}", e);
        }
    }

    // 3. Test FramePacer Shared Memory IPC (Local\FramepacerIPC_v5)
    println!("Testing direct Shared Memory IPC (Local\\FramepacerIPC_v5)...");
    let mut ipc = FramepacerIpc::new();
    match ipc.connect() {
        Ok(()) => {
            println!("  [OK] Connected to FramePacer IPC successfully.");
            match ipc.get_state() {
                Ok((enabled, fps)) => {
                    println!(
                        "  [OK] Current IPC state: limiter_enabled={}, target_fps={}",
                        enabled, fps
                    );
                }
                Err(e) => {
                    println!("  [WARN] Failed to read IPC state: {}", e);
                }
            }
        }
        Err(e) => {
            println!(
                "  [INFO] FramePacer IPC not accessible: {}. (Expected if FramePacer is not currently running)",
                e
            );
        }
    }

    println!("Probe complete.");
}
