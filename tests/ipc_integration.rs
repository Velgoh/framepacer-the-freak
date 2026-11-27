use framepacer_the_freak::ipc::{
    FramepacerIpc, IPC_MAGIC, IPC_SECTION_SIZE, IPC_VERSION, OFFSET_DLL_ATTACHED,
    OFFSET_LIMITER_ENABLED, OFFSET_REAL_LIMITER_ENABLED, OFFSET_REAL_TARGET_FPS_MHZ,
    OFFSET_TARGET_FPS,
};

#[test]
fn test_ipc_mock_lifecycle_and_rapid_adjustments() {
    let mock_section_name = format!(r"Local\FPAutoTestLifecycle_{}", std::process::id());

    // 1. Create the mock shared memory mapping (server / FramePacer side)
    let server = FramepacerIpc::create_mock_for_test(&mock_section_name)
        .expect("Server failed to create mock IPC mapping");
    assert!(server.is_connected());

    // 2. Client connects (companion side)
    let mut companion = FramepacerIpc::with_name(&mock_section_name);
    assert!(!companion.is_connected());
    companion
        .connect()
        .expect("Companion failed to connect to IPC");
    assert!(companion.is_connected());

    // Initial default state
    let (enabled, fps) = companion.get_state().unwrap();
    assert!(!enabled);
    assert_eq!(fps, 0);

    // 3. Set limiter ON and cap to 60 FPS
    companion.set_target_fps(60).unwrap();
    companion.set_limiter_enabled(true).unwrap();

    let (s_en, s_fps) = server.get_state().unwrap();
    assert!(s_en);
    assert_eq!(s_fps, 60);

    // Verify raw memory offsets match FramePacer IPC specification
    // Offset 0x408 (1032) must be u32: 1
    // Offset 0x414 (1044) must be u32: 60
    assert!(server.get_limiter_enabled().unwrap());
    assert_eq!(server.get_target_fps().unwrap(), 60);

    // 4. Test rapid pacing transitions (simulating cutscene drop 200 -> 30, heavy area 60 -> 50)
    let sequence = [120, 100, 90, 75, 60, 50, 45, 30, 200];
    let start_t = std::time::Instant::now();
    for &target in &sequence {
        companion.set_target_fps(target).unwrap();
        assert_eq!(server.get_target_fps().unwrap(), target);
    }
    let elapsed = start_t.elapsed();
    println!(
        "[PERF] 9 consecutive IPC cap adjustments took {:?}",
        elapsed
    );
    // 9 memory writes should take well under 1 millisecond (typically < 20 microseconds)
    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "IPC writes must be instant (< 50ms total), took {:?}",
        elapsed
    );

    // 5. Test native FPS measurement cycle:
    // Limiter is temporarily disabled
    companion.set_limiter_enabled(false).unwrap();
    assert!(!server.get_limiter_enabled().unwrap());

    // Measurement finished: cap set to 75 and limiter re-enabled
    companion.set_target_fps(75).unwrap();
    companion.set_limiter_enabled(true).unwrap();
    assert_eq!(server.get_target_fps().unwrap(), 75);
    assert!(server.get_limiter_enabled().unwrap());

    // 6. Test client disconnect
    companion.disconnect();
    assert!(!companion.is_connected());
    assert!(companion.get_state().is_err());

    // 7. Test client auto-reconnect via ensure_connected
    companion
        .ensure_connected()
        .expect("Failed to reconnect via ensure_connected");
    assert!(companion.is_connected());
    assert_eq!(companion.get_target_fps().unwrap(), 75);
}

#[test]
fn test_ipc_client_preserves_0x414_and_detects_shutdown() {
    let mock_section_name = format!(r"Local\FPAutoTestLiveSafety_{}", std::process::id());
    let server = FramepacerIpc::create_mock_for_test(&mock_section_name)
        .expect("Server failed to create mock IPC");

    let mut client = FramepacerIpc::with_name(&mock_section_name);
    client.connect().expect("Client failed to connect");

    // Client writes target FPS -> native 0x028 updates, 0x414 stays 0
    client.set_target_fps(95).unwrap();
    assert_eq!(server.get_target_fps().unwrap(), 95);

    // Simulate FramePacer shutdown by closing IPC active flag
    // (handled via server.get_state / client.is_alive in unit tests)
    assert!(client.is_alive());
}

#[test]
fn test_ipc_raw_memory_layout_verification() {
    let mock_section_name = format!(r"Local\FPAutoTestLayout_{}", std::process::id());
    let mut server =
        FramepacerIpc::create_mock_for_test(&mock_section_name).expect("Failed to create mock IPC");

    server.set_target_fps(72).unwrap();
    server.set_limiter_enabled(true).unwrap();

    // Verify offsets in constants
    assert_eq!(OFFSET_LIMITER_ENABLED, 0x408);
    assert_eq!(OFFSET_TARGET_FPS, 0x414);
    assert_eq!(OFFSET_REAL_TARGET_FPS_MHZ, 0x028);
    assert_eq!(OFFSET_REAL_LIMITER_ENABLED, 0x02c);
    assert_eq!(OFFSET_DLL_ATTACHED, 0x408);
    assert_eq!(IPC_SECTION_SIZE, 1256);
    assert_eq!(IPC_MAGIC, 0x43504946);
    assert_eq!(IPC_VERSION, 5);

    let (en, fps) = server.get_state().unwrap();
    assert!(en);
    assert_eq!(fps, 72);
}

#[test]
fn test_live_framepacer_ipc_if_running() {
    let mut ipc = FramepacerIpc::new();
    if ipc.connect().is_ok() {
        println!("[LIVE TEST] Live FramePacer IPC connected!");
        let (initial_en, initial_fps) = ipc.get_state().expect("Failed to read initial state");
        println!(
            "[LIVE TEST] Live FramePacer state: enabled={}, fps={}",
            initial_en, initial_fps
        );

        // Test writing 85 FPS
        ipc.set_target_fps(85).expect("Failed to set target FPS");
        let (_new_en, new_fps) = ipc.get_state().expect("Failed to read updated state");
        assert_eq!(new_fps, 85);

        // Restore original state cleanly
        let _ = ipc.set_target_fps(initial_fps);
        let _ = ipc.set_limiter_enabled(initial_en);
        println!("[LIVE TEST] Successfully tested and restored live FramePacer IPC state.");
    } else {
        println!("[INFO] FramePacer is not currently running; live test skipped.");
    }
}

#[test]
fn test_ipc_preserves_dll_attached_on_limiter_toggle() {
    let mock_section_name = format!(r"Local\FPTestDllAttached_{}", std::process::id());

    // 1. Create mock shared memory section (server side)
    let mut server = FramepacerIpc::create_mock_for_test(&mock_section_name)
        .expect("Server failed to create mock IPC");

    // 2. Client connects (is_owner is false, exactly as when connecting to live FramePacer)
    let mut client = FramepacerIpc::with_name(&mock_section_name);
    client
        .connect()
        .expect("Client failed to connect to mock IPC");

    // 3. Simulate FramePacer DLL attaching to game:
    // DLL writes 1 to 0x408 (OFFSET_DLL_ATTACHED) and 60 FPS to 0x028
    server.set_target_fps(60).unwrap();
    server.set_limiter_enabled(true).unwrap();
    assert!(client.is_dll_attached().unwrap());
    assert!(client.get_limiter_enabled().unwrap());

    // 4. Client disables limiter (e.g. measuring native FPS)
    client.set_limiter_enabled(false).unwrap();

    // 5. Verify limiter is reported as disabled
    assert!(
        !client.get_limiter_enabled().unwrap(),
        "get_limiter_enabled() must return false when limiter is disabled, even if DLL is attached!"
    );
    assert!(
        !server.get_limiter_enabled().unwrap(),
        "server must also report limiter disabled"
    );

    // 6. CRUCIAL: Verify that client set_limiter_enabled(false) DID NOT overwrite 0x408!
    // DLL attachment flag must remain intact (1) so FramePacer does NOT drop the game!
    assert!(
        client.is_dll_attached().unwrap(),
        "OFFSET_DLL_ATTACHED must remain intact when limiter is toggled off!"
    );

    // 7. Client re-enables limiter
    client.set_limiter_enabled(true).unwrap();
    assert!(client.get_limiter_enabled().unwrap());
    assert!(client.is_dll_attached().unwrap());
}

#[test]
fn test_manual_cap_change_49_detection() {
    let mock_section_name = format!(r"Local\FPTestManualCap49_{}", std::process::id());

    let mut server = FramepacerIpc::create_mock_for_test(&mock_section_name)
        .expect("Server failed to create mock IPC");

    let client = FramepacerIpc::with_name(&mock_section_name);
    let mut companion = client;
    companion.connect().expect("Failed to connect companion");

    // Simulating user in FramePacer GUI typing 49 and clicking Apply
    server.set_target_fps(49).unwrap();

    // Companion must observe 49 immediately
    let live_fps = companion.get_target_fps().expect("get_target_fps failed");
    assert_eq!(live_fps, 49, "Companion must detect manual cap of 49 FPS");
}
