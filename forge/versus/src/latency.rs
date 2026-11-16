//! Key-to-screen latency without a camera and without taking the keyboard.
//!
//!   versus latency --out FILE.json [--n 60] [--mode keydown|char]
//!                  [--ready FILE] -- TERMINAL COMMAND LINE
//!
//! The terminal is started in a job (run.rs) with `versus echo` as its
//! program. Its window is captured with Windows Graphics Capture, which
//! delivers a frame each time the compositor has new content for that
//! window, stamped with SystemRelativeTime (QPC time, 100 ns units).
//!
//! A key cannot be sent with SendInput: that goes to the foreground window,
//! and a person is typing in another window on this machine. The key is
//! posted to the terminal's window instead (WM_KEYDOWN and WM_KEYUP, which
//! the terminal's own message loop translates, or WM_CHAR with --mode char),
//! so no window is activated. The latency is from just before the post to
//! the SystemRelativeTime of the first captured frame whose pixels differ
//! from the frame before the key: key to compositor frame. It skips the
//! hardware and the input stack before the window's queue (the same for
//! every terminal) and the display's scan-out after composition.
//!
//!   versus echo --ready FILE
//! The program inside: raw console input; each key repaints a 60x3 block of
//! cells with the next letter, cursor hidden, so the only thing on screen
//! that changes is the answer to a key.
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CHAR,
    WM_KEYDOWN, WM_KEYUP,
};

use crate::run::Job;
use crate::{js, opt, opt_num, percentile, qpc, qpf};

fn class_of(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

unsafe extern "system" fn collect(h: HWND, l: LPARAM) -> BOOL {
    let v = &mut *(l.0 as *mut Vec<HWND>);
    v.push(h);
    BOOL(1)
}

/// The largest visible top-level window owned by a process in the job.
fn find_window(job: &Job) -> Option<HWND> {
    let pids = job.pids();
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize));
    }
    let mut best: Option<(i64, HWND)> = None;
    for h in all {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if !pids.contains(&pid) || !unsafe { IsWindowVisible(h) }.as_bool() {
            continue;
        }
        let mut r = Default::default();
        if unsafe { GetWindowRect(h, &mut r) }.is_err() {
            continue;
        }
        let area = ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64);
        if area > 200 * 100 && best.map(|b| area > b.0).unwrap_or(true) {
            best = Some((area, h));
        }
    }
    best.map(|b| b.1)
}

/// Where keys go: a XAML island's input site if the window has one
/// (Windows Terminal), otherwise the window itself.
fn key_target(top: HWND) -> HWND {
    let mut kids: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(top, Some(collect), LPARAM(&mut kids as *mut _ as isize));
    }
    kids.into_iter().find(|&h| class_of(h) == "Windows.UI.Input.InputSite.WindowClass").unwrap_or(top)
}

struct Capture {
    ctx: ID3D11DeviceContext,
    device: ID3D11Device,
    pool: Direct3D11CaptureFramePool,
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl Capture {
    fn new(hwnd: HWND) -> windows::core::Result<Capture> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
            let device: ID3D11Device = device.unwrap();
            let dxgi: IDXGIDevice = device.cast()?;
            let d3d: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size()?)?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(false);
            session.StartCapture()?;
            Ok(Capture { ctx: ctx.unwrap(), device, pool, _session: session, staging: None })
        }
    }

    /// The next frame, if one has arrived: (SystemRelativeTime in 100 ns, BGRA pixels, width).
    fn next(&mut self) -> Option<(i64, Vec<u8>, u32)> {
        let frame = self.pool.TryGetNextFrame().ok()?;
        let t = frame.SystemRelativeTime().ok()?.Duration;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface().ok()?.cast().ok()?;
        let tex: ID3D11Texture2D = unsafe { access.GetInterface().ok()? };
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        if self.staging.as_ref().map(|s| (s.1, s.2) != (d.Width, d.Height)).unwrap_or(true) {
            let mut sd = d;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            sd.BindFlags = 0;
            sd.MiscFlags = 0;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            let mut st = None;
            unsafe { self.device.CreateTexture2D(&sd, None, Some(&mut st)).ok()? };
            self.staging = Some((st?, d.Width, d.Height));
        }
        let st = &self.staging.as_ref()?.0;
        let mut px = vec![0u8; (d.Width * d.Height * 4) as usize];
        unsafe {
            self.ctx.CopyResource(st, &tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
            for y in 0..d.Height as usize {
                let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), d.Width as usize * 4);
                px[y * d.Width as usize * 4..(y + 1) * d.Width as usize * 4].copy_from_slice(src);
            }
            self.ctx.Unmap(st, 0);
        }
        Some((t, px, d.Width))
    }
}

/// Pixels that differ by more than a small amount in any channel.
fn changed(a: &[u8], b: &[u8]) -> usize {
    if a.len() != b.len() {
        return usize::MAX;
    }
    a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(p, q)| (0..3).any(|i| p[i].abs_diff(q[i]) > 24)).count()
}

pub fn main(args: &[String]) -> Result<(), String> {
    let sep = args.iter().position(|a| a == "--").ok_or("latency: -- COMMAND LINE is required")?;
    let (opts, cmd) = (&args[..sep], &args[sep + 1..]);
    let cmdline = if cmd.len() == 1 { cmd[0].clone() } else { cmd.join(" ") };
    let out = opt(opts, "--out").ok_or("--out FILE is required")?.to_string();
    let ready = opt(opts, "--ready").ok_or("--ready FILE is required")?.to_string();
    let n: usize = opt_num(opts, "--n", 60);
    let mode = opt(opts, "--mode").unwrap_or("keydown").to_string();
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mut job = Job::new()?;
    job.spawn(&cmdline)?;
    let result = measure(&mut job, &ready, n, &mode);
    job.terminate();
    let json = match result {
        Ok((lat, misses, cls, w, frames, no_base)) => {
            let mut s = lat.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            format!(
                "{{\"cmdline\":{},\"mode\":{},\"target_class\":{},\"window_px\":{w},\"frames_seen\":{frames},\"misses_without_a_frame\":{no_base},\"n\":{n},\"answered\":{},\"misses\":{misses},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"max_ms\":{:.2},\"min_ms\":{:.2},\"samples_ms\":[{}]}}\n",
                js(&cmdline),
                js(&mode),
                js(&cls),
                s.len(),
                percentile(&s, 0.5),
                percentile(&s, 0.95),
                percentile(&s, 1.0),
                percentile(&s, 0.0),
                lat.iter().map(|x| format!("{x:.2}")).collect::<Vec<_>>().join(",")
            )
        }
        Err(e) => format!("{{\"cmdline\":{},\"error\":{}}}\n", js(&cmdline), js(&e)),
    };
    std::fs::write(&out, json).map_err(|e| format!("{out}: {e}"))?;
    Ok(())
}

fn measure(job: &mut Job, ready: &str, n: usize, mode: &str) -> Result<(Vec<f64>, usize, String, u32, usize, usize), String> {
    // The program inside says when it is reading keys.
    let t = Instant::now();
    while !std::fs::metadata(ready).map(|m| m.len() > 0).unwrap_or(false) {
        if t.elapsed() > Duration::from_secs(30) {
            return Err("the echo program did not start".into());
        }
        job.sample();
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut top = None;
    while top.is_none() && t.elapsed() < Duration::from_secs(30) {
        top = find_window(job);
        std::thread::sleep(Duration::from_millis(50));
    }
    let top = top.ok_or("no window")?;
    // Let start-up drawing finish.
    std::thread::sleep(Duration::from_millis(2500));
    measure_window(top, n, mode)
}

/// Post keys to `top` (or its input site) and time the first changed frame.
pub fn measure_window(top: HWND, n: usize, mode: &str) -> Result<(Vec<f64>, usize, String, u32, usize, usize), String> {
    let target = key_target(top);
    let cls = class_of(target);
    let mut cap = Capture::new(top).map_err(|e| format!("capture: {e}"))?;
    let to100ns = |q: i64| (q as i128 * 10_000_000 / qpf() as i128) as i64;

    let mut last: Option<Vec<u8>> = None;
    let mut width = 0;
    let mut lat = Vec::new();
    let mut misses = 0;
    let mut frames = 0usize;
    let mut no_base = 0usize;
    let mut arrival: Vec<f64> = Vec::new();
    for i in 0..n {
        // Spacing varies so the key lands at different phases of the refresh.
        let gap = Duration::from_millis(180 + (i as u64 * 37) % 61);
        let until = Instant::now() + gap;
        while Instant::now() < until {
            while let Some((_, px, w)) = cap.next() {
                frames += 1;
                last = Some(px);
                width = w;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let Some(base) = last.clone() else {
            misses += 1;
            no_base += 1;
            continue;
        };
        let (vk, scan) = (0x58usize, 0x2Disize); // 'X'
        let k0 = qpc();
        unsafe {
            if mode == "char" {
                let _ = PostMessageW(target, WM_CHAR, WPARAM('x' as usize), LPARAM(1 | scan << 16));
            } else {
                // Key up follows 40 ms later, as when a person types: a
                // down and up queued together reads as synthetic to winit.
                let _ = PostMessageW(target, WM_KEYDOWN, WPARAM(vk), LPARAM(1 | scan << 16));
            }
        }
        let mut up_due = (mode != "char").then(|| Instant::now() + Duration::from_millis(40));
        let k100 = to100ns(k0);
        let deadline = Instant::now() + Duration::from_millis(1000);
        let mut hit = None;
        while Instant::now() < deadline && (hit.is_none() || up_due.is_some()) {
            if up_due.map(|t| Instant::now() >= t).unwrap_or(false) {
                unsafe {
                    let _ = PostMessageW(target, WM_KEYUP, WPARAM(vk), LPARAM(1 | scan << 16 | 1 << 30 | 1 << 31));
                }
                up_due = None;
            }
            if hit.is_some() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            while let Some((ft, px, w)) = cap.next() {
                frames += 1;
                width = w;
                if ft >= k100 && changed(&base, &px) >= 20 {
                    hit = Some(ft);
                    arrival.push((to100ns(qpc()) - ft) as f64 / 10_000.0);
                }
                last = Some(px);
                if hit.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_micros(500));
        }
        match hit {
            Some(ft) => lat.push((ft - k100) as f64 / 10_000.0),
            None => misses += 1,
        }
    }
    arrival.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let cls = format!("{cls} (capture delivery after frame time p50 {:.2} ms, max {:.2})", percentile(&arrival, 0.5), percentile(&arrival, 1.0));
    Ok((lat, misses, cls, width, frames, no_base))
}

/// The program inside the terminal.
pub fn echo(args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, SetConsoleMode, SetConsoleOutputCP, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let ready = opt(args, "--ready").ok_or("--ready FILE is required")?;
    let (hin, hout) = unsafe { (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE)) };
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleMode(hout, ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        SetConsoleMode(hin, 0);
    }
    let put = |s: &str| {
        use std::io::Write;
        let mut o = std::io::stdout();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    };
    put("\x1b[?25l\x1b[2J\x1b[1;1Hversus echo: each key repaints the block below\r\n");
    std::fs::write(ready, "ready").map_err(|e| e.to_string())?;
    let mut count = 0usize;
    let mut rec: [INPUT_RECORD; 16] = unsafe { std::mem::zeroed() };
    loop {
        let mut got = 0u32;
        if unsafe { ReadConsoleInputW(hin, rec.as_mut_ptr(), 16, &mut got) } == 0 {
            return Err("ReadConsoleInput failed".into());
        }
        let mut redraw = false;
        for r in &rec[..got as usize] {
            if r.EventType as u32 == KEY_EVENT as u32 {
                let k = unsafe { r.Event.KeyEvent };
                if k.bKeyDown != 0 && unsafe { k.uChar.UnicodeChar } != 0 {
                    count += 1;
                    redraw = true;
                }
            }
        }
        if redraw {
            let c = (b'A' + (count % 26) as u8) as char;
            let row: String = std::iter::repeat(c).take(60).collect();
            put(&format!("\x1b[3;3H{row}\x1b[4;3H{row}\x1b[5;3H{row}"));
        }
    }
}

/// Calibration: a window of this process that repaints in its WM_KEYDOWN
/// handler (GDI, no other work). Whatever the method reports for it is the
/// floor that the posting, the compositor and the capture add by themselves.
pub fn calib(args: &[String]) -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, LRESULT};
    use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, UpdateWindow, PAINTSTRUCT};
    use windows::Win32::UI::WindowsAndMessaging::*;
    static mut COUNT: u32 = 0;
    unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match m {
            WM_KEYDOWN => {
                COUNT += 1;
                let _ = InvalidateRect(h, None, false);
                let _ = UpdateWindow(h);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(h, &mut ps);
                let c = if COUNT % 2 == 0 { 0x202020 } else { 0xE0E0E0 };
                let b = CreateSolidBrush(COLORREF(c));
                FillRect(dc, &ps.rcPaint, b);
                let _ = DeleteObject(b);
                let _ = EndPaint(h, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
    let out = opt(args, "--out").ok_or("--out FILE is required")?.to_string();
    let n: usize = opt_num(args, "--n", 40);
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    std::thread::spawn(move || unsafe {
        let wc = WNDCLASSW { lpfnWndProc: Some(proc_), lpszClassName: w!("VersusCalib"), ..Default::default() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(WINDOW_EX_STYLE(0), w!("VersusCalib"), w!("versus calibration"), WS_OVERLAPPEDWINDOW, 100, 100, 900, 600, None, None, None, None).unwrap();
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        tx.send(h.0 as isize).unwrap();
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
    let h = HWND(rx.recv().unwrap() as *mut _);
    std::thread::sleep(Duration::from_millis(1000));
    let r = measure_window(h, n, "keydown");
    let json = match r {
        Ok((lat, misses, cls, w, frames, _)) => {
            let mut s = lat.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            format!(
                "{{\"calibration\":true,\"note\":{},\"window_px\":{w},\"frames_seen\":{frames},\"n\":{n},\"answered\":{},\"misses\":{misses},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"max_ms\":{:.2},\"min_ms\":{:.2}}}\n",
                js(&cls), s.len(), percentile(&s, 0.5), percentile(&s, 0.95), percentile(&s, 1.0), percentile(&s, 0.0)
            )
        }
        Err(e) => format!("{{\"calibration\":true,\"error\":{}}}\n", js(&e)),
    };
    std::fs::write(&out, json).map_err(|e| e.to_string())?;
    Ok(())
}
