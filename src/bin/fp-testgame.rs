use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut duration_secs: Option<f64> = None;
    let mut fixed_fps: Option<f64> = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--duration" && i + 1 < args.len() {
            duration_secs = args[i + 1].parse().ok();
            i += 2;
        } else if args[i] == "--fps" && i + 1 < args.len() {
            fixed_fps = args[i + 1].parse().ok();
            i += 2;
        } else {
            i += 1;
        }
    }

    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class_name = w!("FPTestGameClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("FramePacer Test Game"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            800,
            600,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )?;

        // Ensure window has foreground focus so FramePacer hotkeys are directed to it
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = BringWindowToTop(hwnd);
        let fg_hwnd = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg_hwnd, None);
        let my_thread = GetCurrentThreadId();

        if fg_thread != my_thread && fg_thread != 0 {
            let _ = AttachThreadInput(my_thread, fg_thread, true);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            let _ = AttachThreadInput(my_thread, fg_thread, false);
        } else {
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
        }

        let swap_desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 800,
                Height: 600,
                RefreshRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: BOOL(1),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Flags: 0,
        };

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut swap_chain: Option<IDXGISwapChain> = None;
        let mut feature_level = D3D_FEATURE_LEVEL_11_0;

        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&swap_desc),
            Some(&mut swap_chain),
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )?;

        let swap_chain = swap_chain.unwrap();
        let context = context.unwrap();
        let device = device.unwrap();

        let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))?;
        let rtv = rtv.unwrap();

        println!(
            "fp-testgame running (PID {}). Duration: {:?}, Fixed FPS: {:?}",
            std::process::id(),
            duration_secs,
            fixed_fps
        );

        let mut msg = MSG::default();
        let mut frame_count: u64 = 0;
        let start = Instant::now();

        while msg.message != WM_QUIT {
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                let elapsed = start.elapsed().as_secs_f64();
                if let Some(dur) = duration_secs
                    && elapsed >= dur
                {
                    println!(
                        "fp-testgame reached duration limit ({:.1}s), frames rendered: {}, exiting.",
                        dur, frame_count
                    );
                    break;
                }

                let target_fps = if let Some(fps) = fixed_fps {
                    fps
                } else if elapsed < 4.0 {
                    200.0
                } else if elapsed < 8.0 {
                    30.0
                } else if elapsed < 12.0 {
                    50.0
                } else {
                    200.0
                };

                let frame_start = Instant::now();
                let color = [((frame_count % 255) as f32) / 255.0, 0.2, 0.6, 1.0];
                context.ClearRenderTargetView(&rtv, &color);
                swap_chain.Present(0, DXGI_PRESENT(0)).ok()?;
                frame_count += 1;

                let target_frame_time = Duration::from_secs_f64(1.0 / target_fps);
                let render_time = frame_start.elapsed();
                if render_time < target_frame_time {
                    std::thread::sleep(target_frame_time - render_time);
                }
            }
        }
        Ok(())
    }
}
