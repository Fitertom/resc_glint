//! Run the "open" verb of a class on files the way Explorer does (ShellExecuteEx), and print
//! the wall clock at each call, to line up with Glint's trace (`GLINT_TRACE=1`). The shell is
//! warm in Explorer, so the first call here only warms it up; the window is closed after each.
//! `cargo run --release --example shell_open -- <class> <files...>`, e.g. `Glint.Image a.png b.jpg`.

use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::UI::Shell::{SEE_MASK_CLASSNAME, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, SW_SHOWNORMAL, WM_CLOSE};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn wall() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0 % 100_000.0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [class, files @ ..] = &args[..] else {
        eprintln!("usage: shell_open <class> <files...>");
        std::process::exit(2);
    };
    // SAFETY: plain calls with strings that outlive them.
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let (class, verb, window) = (wide(class), wide("open"), wide("Glint"));
        for f in files {
            let file = wide(f);
            let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
            sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            sei.fMask = SEE_MASK_CLASSNAME | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
            sei.lpClass = class.as_ptr();
            sei.lpVerb = verb.as_ptr();
            sei.lpFile = file.as_ptr();
            sei.nShow = SW_SHOWNORMAL as i32;
            let t = wall();
            let ok = ShellExecuteExW(&mut sei);
            println!("{t:.3}|{ok}|{:.2}|{f}", wall() - t);
            std::thread::sleep(std::time::Duration::from_millis(1200));
            PostMessageW(FindWindowW(window.as_ptr(), std::ptr::null()), WM_CLOSE, 0, 0);
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    }
}
