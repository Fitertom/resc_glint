//! Glint — a fast image viewer: Win32 + Vulkan, decoding through the system's WIC.
//!
//! Start-up is ordered for the first picture: the decoding threads start and are given the
//! file before the window exists; the window shows at once in the background colour; Vulkan
//! comes up while the codec works; the folder is listed on its own thread.

#![windows_subsystem = "windows"]

mod app;
mod assoc;
mod clipboard;
mod com;
mod com_server;
mod config;
mod files;
mod gpu;
mod install;
mod loader;
mod thumbcache;
mod ui;
mod wic;
mod win;

use std::path::PathBuf;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, MB_ICONERROR, MB_OK, MessageBoxW, SM_CXSCREEN, SM_CYSCREEN};

/// Start-up timing, written to `%TEMP%\glint_trace.txt` when `GLINT_TRACE` is set.
static START: std::sync::OnceLock<(std::time::Instant, bool)> = std::sync::OnceLock::new();

pub fn trace(what: &str) {
    let Some(&(t0, true)) = START.get() else { return };
    // THE LINE GOES TO A WRITER THREAD: opening and appending to the file right here cost up
    // to milliseconds per line, and distorted the very timings being traced.
    static TX: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<String>>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            use std::io::Write;
            let path = std::env::temp_dir().join("glint_trace.txt");
            while let Ok(line) = rx.recv() {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    let _ = writeln!(f, "{line}");
                    while let Ok(more) = rx.try_recv() {
                        let _ = writeln!(f, "{more}");
                    }
                }
            }
        });
        std::sync::Mutex::new(tx)
    });
    // The wall clock too (precise to the microsecond): lines from different processes line up.
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0) % 100_000.0;
    let _ = tx.lock().unwrap().send(format!("{wall:10.3} | {:8.2} ms  {what}", t0.elapsed().as_secs_f64() * 1000.0));
}

fn main() {
    START.get_or_init(|| (std::time::Instant::now(), std::env::var_os("GLINT_TRACE").is_some()));
    #[cfg(feature = "crashlog")]
    crashlog::install();
    let arg = std::env::args_os().nth(1);
    // COM starts the server as `glint.exe -Embedding` when Explorer asks for it and no copy runs.
    let flag = arg.as_ref().and_then(|a| a.to_str()).filter(|a| a.starts_with("--") || a.eq_ignore_ascii_case("-Embedding") || a.eq_ignore_ascii_case("/Embedding")).map(|a| {
        if a.starts_with("--") { a.to_owned() } else { "--background".to_owned() }
    });
    // FIRST OF ALL: a resident viewer takes the file, and this launch is done in a few ms.
    if flag.is_none()
        && let Some(host) = win::find_host()
        && win::send_to_host(host, arg.as_deref())
    {
        trace("handed to the resident process");
        return;
    }
    match flag.as_deref() {
        Some("--uninstall") => return report(install::uninstall(), "Glint is uninstalled."),
        Some("--register") => {
            let exe = std::env::current_exe().unwrap_or_default();
            let r = assoc::set_autostart(true, &exe).and_then(|_| assoc::set_default(true, &exe));
            let _ = std::process::Command::new(std::env::current_exe().unwrap_or_default()).arg("--background").spawn();
            config::store("setup_done", "true");
            return report(r.map(|_| ()), "Glint starts with Windows and opens images now.");
        }
        Some("--unregister") => {
            let exe = std::env::current_exe().unwrap_or_default();
            let r = assoc::set_autostart(false, &exe).and_then(|_| assoc::set_default(false, &exe));
            if let Some(h) = win::find_host() {
                // SAFETY: posting to a window handle.
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(h, win::WM_QUIT_HOST, 0, 0) };
            }
            return report(r.map(|_| ()), "Glint is removed from autostart and from image types.");
        }
        Some("--quit") => {
            if let Some(h) = win::find_host() {
                // SAFETY: posting to a window handle.
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(h, win::WM_QUIT_HOST, 0, 0) };
            }
            return;
        }
        _ => {}
    }
    let background = flag.as_deref() == Some("--background");
    if background && win::find_host().is_some() {
        return;
    }
    let arg = if flag.is_some() { None } else { arg };
    let cfg = config::Config::load();
    if !cfg.implicit_layers {
        // SAFETY: no other thread exists yet to read the environment.
        unsafe { std::env::set_var("VK_LOADER_LAYERS_DISABLE", "~implicit~") };
    }
    win::dpi_aware();
    // SAFETY: plain queries.
    let screen = unsafe { (GetSystemMetrics(SM_CXSCREEN).max(640) as u32, GetSystemMetrics(SM_CYSCREEN).max(480) as u32) };
    let loader = loader::Loader::start(screen);
    let nav = match arg {
        Some(p) => app::Nav::open(&loader, PathBuf::from(p), 0),
        None => app::Nav::empty(),
    };
    win::set_resident(cfg.resident);
    let hwnd = match win::create("Glint", cfg.colorref(), cfg.window == config::WindowMode::Maximized, !background, cfg.placement) {
        Ok(h) => h,
        Err(e) => return fail(&e),
    };
    trace("window shown");
    loader.set_hwnd(hwnd as isize);
    // THE RESIDENT COPY SERVES EXPLORER'S "OPEN" ITSELF (`com_server`): a double click then
    // starts no process at all.
    if cfg.resident {
        com_server::register();
    }
    let gpu = match gpu::Gpu::new(hwnd as isize, win::client_size(hwnd), cfg.prefer_integrated) {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };
    trace("vulkan ready");
    let mut app = app::App::new(hwnd, gpu, loader, cfg, nav);
    if background {
        app.park();
        app.settle();
    }
    win::run(&mut app);
    // Whichever way the loop ended, the tray icon goes with the process: left behind it would
    // sit there dead until the pointer passed over it.
    win::tray_remove(hwnd);
    if let Some((exe, args)) = app.relaunch.take() {
        let _ = std::process::Command::new(exe).args(args).spawn();
    }
    // Straight out: the decoding threads sleep on a condition variable, and freeing a cache of
    // photos page by page would only delay the exit.
    drop(app);
    std::process::exit(0);
}

fn message(text: &str, flags: u32) {
    let (t, c) = (win::wide(text), win::wide("Glint"));
    // SAFETY: both strings live through the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), flags) };
}

fn fail(e: &str) {
    message(e, MB_OK | MB_ICONERROR);
}

fn report(r: Result<(), String>, ok: &str) {
    match r {
        Ok(()) => message(ok, MB_OK),
        Err(e) => fail(&e),
    }
}

#[cfg(feature = "crashlog")]
mod crashlog {
    type Filter = unsafe extern "system" fn(*const std::ffi::c_void) -> i32;
    windows_link::link!("kernel32.dll" "system" fn SetUnhandledExceptionFilter(f: Option<Filter>) -> Option<Filter>);
    unsafe extern "system" fn on_crash(info: *const std::ffi::c_void) -> i32 {
        // EXCEPTION_POINTERS -> EXCEPTION_RECORD: code, flags, record, address
        let rec = unsafe { *(info as *const *const u8) };
        let code = unsafe { *(rec as *const u32) };
        let addr = unsafe { *(rec.add(16) as *const usize) };
        crate::trace(&format!("CRASH 0x{code:08X} at 0x{addr:X} on thread {:?}
{}", std::thread::current().name(), std::backtrace::Backtrace::force_capture()));
        0
    }
    pub fn install() {
        unsafe { SetUnhandledExceptionFilter(Some(on_crash)) };
    }
}
