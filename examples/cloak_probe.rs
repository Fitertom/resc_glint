//! Probe: does a DWM-cloaked window take clicks, and what do show / uncloak cost?
//! `cargo run --release --example cloak_probe`

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn pump() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() {
    unsafe {
        let class = wide("cloak_probe");
        let wc = WNDCLASSW { lpfnWndProc: Some(proc_), hInstance: GetModuleHandleW(std::ptr::null()), lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
        RegisterClassW(&wc);
        let h = CreateWindowExW(0, class.as_ptr(), wide("probe").as_ptr(), WS_OVERLAPPEDWINDOW, 100, 100, 800, 600, std::ptr::null_mut(), std::ptr::null_mut(), wc.hInstance, std::ptr::null());
        let cloak = |on: i32| DwmSetWindowAttribute(h, 13, (&on as *const i32).cast(), 4);
        let centre = POINT { x: 500, y: 400 };

        let fg = GetForegroundWindow();
        // Hidden → shown, as the viewer does today.
        let t = std::time::Instant::now();
        ShowWindow(h, SW_SHOWNORMAL);
        println!("ShowWindow from hidden: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        pump();
        ShowWindow(h, SW_HIDE);
        pump();

        // Shown but cloaked.
        cloak(1);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        pump();
        println!("cloaked+shown: foreground unchanged = {}", GetForegroundWindow() == fg);
        println!("cloaked: WindowFromPoint hits our window = {}", WindowFromPoint(centre) == h);
        let t = std::time::Instant::now();
        cloak(0);
        println!("uncloak: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        println!("uncloaked: WindowFromPoint hits our window = {}", WindowFromPoint(centre) == h);
        pump();
        let t = std::time::Instant::now();
        cloak(1);
        println!("cloak: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        pump();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            cloak(0);
            let a = t.elapsed();
            pump();
            cloak(1);
            pump();
            println!("uncloak again: {:.2} ms", a.as_secs_f64() * 1e3);
        }
        DestroyWindow(h);
    }
}
