//! The window: frameless, drawn by us, on Win32 directly.
//!
//! **The window procedure never calls the application re-entrantly; it queues** (as
//! `resc_window` does). Win32 calls it from inside our own calls — `SetWindowPos` delivers
//! `WM_SIZE` before it returns — and the application would be borrowed twice. The queue is
//! drained at once when the application is not running, which is also what keeps frames
//! coming during the system's modal loops: dragging an edge or the caption runs its own loop
//! inside `DispatchMessageW`, and `WM_SIZE` from there draws a frame right away instead of
//! freezing the picture until the mouse is let go.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, UpdateWindow, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT,
    ScreenToClient,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub use windows_sys::Win32::UI::WindowsAndMessaging::{IDC_ARROW, IDC_SIZEALL, IDC_SIZEWE};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Btn {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug)]
pub enum Ev {
    /// The client area's new size; never zero (a minimised window says nothing).
    Resize(u32, u32),
    Key { vk: u16, down: bool, repeat: bool },
    MouseMove(i32, i32),
    MouseLeave,
    Button { b: Btn, down: bool, x: i32, y: i32 },
    DoubleClick(i32, i32),
    /// Wheel notches (positive away from the user), at a client point.
    Wheel { notches: f32, x: i32, y: i32 },
    Drop(PathBuf),
    /// Something arrived from the decoding threads.
    Loaded,
    Paint,
    Dpi(u32),
    /// Another launch handed over its file (`None`: launched with no file) — show the window.
    Open(Option<PathBuf>),
    /// The close button or Alt+F4: hidden when resident, closed otherwise.
    Close,
    /// Exit for real (`--quit`, uninstall).
    Quit,
    /// The tray icon: left click (open), or a command from its menu.
    Tray(TrayCmd),
    /// A timer set with `set_timer` went off.
    Timer(usize),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TrayCmd {
    Open,
    OpenFile,
    Settings,
    Exit,
}

const WM_TRAY: u32 = WM_APP + 5;

fn taskbar_created() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: registers a name, idempotent.
    *ID.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}

/// The icon in the notification area, the exe's own.
pub fn tray_add(hwnd: HWND) {
    use windows_sys::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NOTIFYICONDATAW, Shell_NotifyIconW};
    taskbar_created();
    // SAFETY: the struct is ours and filled here.
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        let side = GetSystemMetrics(SM_CXSMICON);
        nid.hIcon = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, side, side, LR_DEFAULTCOLOR) as HICON;
        for (d, s) in nid.szTip.iter_mut().zip("Glint — click to open the clipboard".encode_utf16()) {
            *d = s;
        }
        Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

pub fn tray_remove(hwnd: HWND) {
    use windows_sys::Win32::UI::Shell::{NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
    // SAFETY: as above.
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// The tray icon's menu, at the pointer; the chosen command, if any.
fn tray_menu(hwnd: HWND) -> Option<TrayCmd> {
    let items = [(1, "Open from clipboard"), (2, "Open file…"), (0, ""), (3, "Settings"), (0, ""), (4, "Exit Glint")];
    // SAFETY: a menu of ours, destroyed below; the window is ours.
    unsafe {
        let menu = CreatePopupMenu();
        for (id, text) in items {
            if id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            } else {
                AppendMenuW(menu, MF_STRING, id, wide(text).as_ptr());
            }
        }
        SetMenuDefaultItem(menu, 1, 0);
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        // The menu closes on a click elsewhere only if our window is in front (a documented
        // quirk of tray menus), and the empty message after it lets the next click through.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.x, p.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd {
            1 => Some(TrayCmd::Open),
            2 => Some(TrayCmd::OpenFile),
            3 => Some(TrayCmd::Settings),
            4 => Some(TrayCmd::Exit),
            _ => None,
        }
    }
}

pub trait Handler {
    fn event(&mut self, e: Ev);
    /// After every batch of events: draw if anything changed.
    fn frame(&mut self);
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Ev>> = const { RefCell::new(VecDeque::new()) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static APP: Cell<Option<*mut dyn Handler>> = const { Cell::new(None) };
    /// Vulkan draws every pixel: past the first frame the class brush must not erase under it,
    /// or every resize flashes it.
    static READY: Cell<bool> = const { Cell::new(false) };
    /// What the application drew, for hit-testing: caption height and the width of the
    /// buttons at its right, in pixels. Zero caption — fullscreen, no edges either.
    static CAPTION: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    static CURSOR: Cell<usize> = const { Cell::new(0) };
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static RESIDENT: Cell<bool> = const { Cell::new(false) };
}

/// Messages between launches: a file for the resident process, and "exit for real".
const COPYDATA_OPEN: usize = 0x5345_4549; // "SEEI"
pub const WM_QUIT_HOST: u32 = WM_APP + 2;
const CLASS: &str = "Glint";
/// A detached window (`--window`) goes under a class of its own: `find_host` looks up the
/// resident process by class, and must never hand a file to a reference window instead.
const CLASS_DETACHED: &str = "Glint.Window";
static DETACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_detached() {
    DETACHED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_detached() -> bool {
    DETACHED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Above every window that is not topmost itself, or back among them.
pub fn set_topmost(hwnd: HWND, on: bool) {
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    // SAFETY: a window of ours.
    unsafe { SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
}

/// The whole window's opacity, 0..1, applied by the compositor. Layered only while below 1:
/// an opaque window stays an ordinary one, and its present path is untouched.
pub fn set_opacity(hwnd: HWND, a: f32) {
    // SAFETY: style bits and an attribute on a window of ours.
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let layered = ex & WS_EX_LAYERED as isize != 0;
        if a >= 0.999 {
            if layered {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as isize));
            }
            return;
        }
        if !layered {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
        }
        SetLayeredWindowAttributes(hwnd, 0, (a.clamp(0.0, 1.0) * 255.0).round() as u8, LWA_ALPHA);
    }
}

/// Size the window to show `w × h` (client pixels, caption included) around its current centre,
/// kept inside the work area of its monitor.
pub fn fit_to(hwnd: HWND, w: i32, h: i32) {
    // SAFETY: queries and a move on a window of ours.
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(hwnd, &mut r);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let wa = mi.rcWork;
        let (w, h) = (w.min(wa.right - wa.left), h.min(wa.bottom - wa.top));
        let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let x = (cx - w / 2).clamp(wa.left, wa.right - w);
        let y = (cy - h / 2).clamp(wa.top, wa.bottom - h);
        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/// The work area of the window's monitor: width and height.
pub fn work_size(hwnd: HWND) -> (i32, i32) {
    // SAFETY: a query filling a struct of ours.
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        (mi.rcWork.right - mi.rcWork.left, mi.rcWork.bottom - mi.rcWork.top)
    }
}

/// A file from Explorer through COM (`com_server`): handled as a hand-over from another launch.
pub fn open_from_com(path: Option<PathBuf>) {
    post(Ev::Open(path));
}

pub fn set_resident(on: bool) {
    RESIDENT.set(on);
}

/// The resident process's window, if one is running.
pub fn find_host() -> Option<HWND> {
    let c = wide(CLASS);
    // SAFETY: a lookup by class name.
    let h = unsafe { FindWindowW(c.as_ptr(), std::ptr::null()) };
    (!h.is_null()).then_some(h)
}

/// Hand `path` to the resident process. The launch the user clicked owns the foreground and
/// passes that right on, or the window would open behind Explorer.
pub fn send_to_host(host: HWND, path: Option<&std::ffi::OsStr>) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    let data: Vec<u16> = path.map_or_else(Vec::new, |p| p.encode_wide().collect());
    // SAFETY: the struct and its data live through the synchronous send.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(host, &mut pid);
        AllowSetForegroundWindow(pid);
        let cds = COPYDATASTRUCT { dwData: COPYDATA_OPEN, cbData: (data.len() * 2) as u32, lpData: data.as_ptr() as *mut _ };
        let mut res = 0;
        // A hung resident must not hang the click: after a second this launch opens by itself.
        SendMessageTimeoutW(host, WM_COPYDATA, 0, &cds as *const _ as isize, SMTO_ABORTIFHUNG, 1000, &mut res) != 0 && res == 1
    }
}

/// Show a hidden window as it was: maximised if it was, else at its own rectangle.
pub fn show(hwnd: HWND, maximized: bool) {
    // SAFETY: a window of ours.
    unsafe {
        ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOW });
        SetForegroundWindow(hwnd);
    }
}

fn cloak(hwnd: HWND, on: bool) {
    let v: i32 = on as i32;
    // SAFETY: DWMWA_CLOAK (13) on a window of ours, a BOOL living through the call.
    unsafe { DwmSetWindowAttribute(hwnd, 13, (&v as *const i32).cast(), 4) };
}

/// Put the window out of sight while keeping it SHOWN: cloaked by the compositor, off the
/// taskbar, not active. Showing a hidden window costs 3–18 ms of `ShowWindow`; taking the cloak
/// off costs 0.1 ms, and the frame under it can be drawn before it comes off. A cloaked window
/// takes no clicks (checked: `WindowFromPoint` goes through it).
pub fn park(hwnd: HWND, maximized: bool) {
    cloak(hwnd, true);
    // SAFETY: a window of ours.
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOWNA });
        }
        // THE KEYBOARD MUST NOT STAY WITH AN INVISIBLE WINDOW: hiding it hands activation to
        // the next window the way the system chooses; shown again unactivated, it stays cloaked.
        if GetForegroundWindow() == hwnd {
            ShowWindow(hwnd, SW_HIDE);
            ShowWindow(hwnd, SW_SHOWNA);
        }
    }
    taskbar_tab(hwnd, false);
}

const WM_ADD_TAB: u32 = WM_APP + 6;

/// Take the cloak off and come to the front, with whatever was last presented. The taskbar
/// button comes back a moment later, through the queue: it is a call into Explorer that took
/// ~7 ms, and nothing on screen waits for it.
pub fn reveal(hwnd: HWND) {
    cloak(hwnd, false);
    crate::trace("    [reveal] uncloaked");
    // SAFETY: a window of ours.
    unsafe {
        SetForegroundWindow(hwnd);
        crate::trace("    [reveal] foreground");
        PostMessageW(hwnd, WM_ADD_TAB, 0, 0);
    }
}

/// The window's taskbar button, through ITaskbarList: a cloaked window is still "shown" and
/// would keep its button.
fn taskbar_tab(hwnd: HWND, on: bool) {
    use crate::com::Com;
    use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows_sys::core::GUID;
    thread_local! {
        static LIST: Option<Com> = {
            const CLSID: GUID = GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
            const IID: GUID = GUID::from_u128(0x56fdf342_fd6d_11d0_958a_006097c9a090);
            // SAFETY: COM on this thread; HrInit is slot 3.
            unsafe {
                CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
                let mut p = std::ptr::null_mut();
                (CoCreateInstance(&CLSID, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID, &mut p) >= 0 && !p.is_null()).then(|| {
                    let c = Com(p);
                    c.slot::<unsafe extern "system" fn(*mut std::ffi::c_void) -> i32>(3)(c.raw());
                    c
                })
            }
        };
    }
    LIST.with(|l| {
        if let Some(l) = l {
            // SAFETY: AddTab is slot 4, DeleteTab slot 5.
            unsafe { l.slot::<unsafe extern "system" fn(*mut std::ffi::c_void, HWND) -> i32>(if on { 4 } else { 5 })(l.raw(), hwnd) };
        }
    });
}

pub fn focus(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

/// Give the pages back while hidden: the driver's and ours move to the standby list and come
/// back on the next show from RAM, as soft faults — the process sits at a few MB meanwhile.
pub fn trim_memory() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: plain call on our own process.
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

pub fn set_ready() {
    READY.set(true);
}

pub fn set_caption(height: i32, buttons: i32) {
    CAPTION.set((height, buttons));
}

/// Once, `ms` from now: `Ev::Timer(id)`. Set again, the same id starts over.
pub fn set_timer(hwnd: HWND, id: usize, ms: u32) {
    // SAFETY: a timer on a window of ours.
    unsafe { SetTimer(hwnd, id, ms, None) };
}

pub fn kill_timer(hwnd: HWND, id: usize) {
    // SAFETY: as above.
    unsafe { KillTimer(hwnd, id) };
}

/// The pointer over the client area (`IDC_*`).
pub fn set_cursor(id: windows_sys::core::PCWSTR) {
    CURSOR.set(id as usize);
    // SAFETY: a system cursor by id.
    unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), id)) };
}

fn post(e: Ev) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        // ONLY THE LAST SIZE COUNTS: each one rebuilds the swapchain.
        if matches!(e, Ev::Resize(..)) {
            q.retain(|o| !matches!(o, Ev::Resize(..)));
        }
        q.push_back(e);
    });
    drain();
}

fn drain() {
    if BUSY.get() {
        return;
    }
    let Some(app) = APP.get() else { return };
    BUSY.set(true);
    loop {
        while let Some(e) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
            // SAFETY: the pointer is set by `run` for the length of the loop, and BUSY keeps
            // this the only borrow.
            unsafe { (*app).event(e) };
        }
        // SAFETY: as above.
        unsafe { (*app).frame() };
        if QUEUE.with(|q| q.borrow().is_empty()) {
            break;
        }
    }
    BUSY.set(false);
}

pub fn run(app: &mut dyn Handler) {
    // SAFETY: the pointer is cleared below, before `app`'s borrow ends.
    let p: *mut (dyn Handler + '_) = app;
    APP.set(Some(unsafe { std::mem::transmute::<*mut (dyn Handler + '_), *mut (dyn Handler + 'static)>(p) }));
    drain();
    // SAFETY: the message is ours and handed straight back.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.set(None);
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Per-monitor dpi, before the first window: without it the system scales our pixels as a
/// bitmap and every line goes soft.
pub fn dpi_aware() {
    // SAFETY: plain call with a constant.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Make the window. Shown at once: the class brush paints the background colour until the first
/// frame, so the user sees the window as soon as the process starts.
pub fn create(title: &str, bg: u32, maximized: bool, visible: bool, placement: Option<([i32; 4], bool)>) -> Result<HWND, String> {
    // SAFETY: every struct is filled here and every string outlives its call.
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(if is_detached() { CLASS_DETACHED } else { CLASS });
        // The exe's own icon resource (group 1, written by `build.rs`), at the sizes the
        // system wants: one file for the exe, the taskbar and the window.
        let icon = |side| LoadImageW(hinst, 1 as _, IMAGE_ICON, side, side, LR_DEFAULTCOLOR) as HICON;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: icon(GetSystemMetrics(SM_CXICON)),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: CreateSolidBrush(bg),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON)),
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(format!("RegisterClassExW: {}", std::io::Error::last_os_error()));
        }
        // Normal size: 3/4 of the work area, centred. Also where a maximised window restores to.
        let mut work = RECT { left: 0, top: 0, right: 1280, bottom: 800 };
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        let (w, h) = (ww * 3 / 4, wh * 3 / 4);
        let t = wide(title);
        // THE FULL OVERLAPPED STYLE, frame and all, with the frame then taken away at
        // `WM_NCCALCSIZE`: a bare popup would maximise over the taskbar, never snap, and get
        // no shadow or rounded corners from the compositor.
        let hwnd = CreateWindowExW(
            WS_EX_ACCEPTFILES | WS_EX_APPWINDOW,
            class.as_ptr(),
            t.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            work.left + (ww - w) / 2,
            work.top + (wh - h) / 2,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW: {}", std::io::Error::last_os_error()));
        }
        let on: i32 = 1;
        let attr = |a: u32, v: &i32| DwmSetWindowAttribute(hwnd, a, (v as *const i32).cast(), 4);
        // DWMWA_TRANSITIONS_FORCEDISABLED: no fade-in on opening, no zoom on minimise, maximise
        // or restore — the window is simply there.
        attr(3, &on);
        attr(20, &on); // DWMWA_USE_IMMERSIVE_DARK_MODE: the system menu and snap previews go dark
        attr(33, &2); // DWMWA_WINDOW_CORNER_PREFERENCE = round
        attr(34, &(crate::ui::CHROME_BG_REF as i32)); // DWMWA_BORDER_COLOR: the caption's own colour
        // A one-pixel frame left to the compositor: that is what it draws the shadow from.
        let m = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 };
        DwmExtendFrameIntoClientArea(hwnd, &m);
        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        // WHERE IT WAS CLOSED, if that monitor is still there; the settings' mode otherwise.
        let (rect, max) = placement.unwrap_or(([0; 4], maximized));
        set_placement(hwnd, rect, max, false);
        if visible {
            ShowWindow(hwnd, if max { SW_SHOWMAXIMIZED } else { SW_SHOW });
            UpdateWindow(hwnd);
        }
        Ok(hwnd)
    }
}

/// The window's normal rectangle and whether it is maximised, as `set_placement` takes them.
pub fn placement(hwnd: HWND) -> ([i32; 4], bool) {
    // SAFETY: a struct of ours filled by the call.
    unsafe {
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        GetWindowPlacement(hwnd, &mut wp);
        let r = wp.rcNormalPosition;
        let max = wp.showCmd == SW_SHOWMAXIMIZED as u32 || (wp.showCmd == SW_HIDE as u32 && wp.flags & WPF_RESTORETOMAXIMIZED != 0);
        ([r.left, r.top, r.right, r.bottom], max)
    }
}

/// Put the window back where it was closed — on that monitor, if it is still there. A rectangle
/// on a monitor since unplugged would open the window off every screen; then it stays where the
/// system put it.
pub fn set_placement(hwnd: HWND, rect: [i32; 4], maximized: bool, show: bool) {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    let r = RECT { left: rect[0], top: rect[1], right: rect[2], bottom: rect[3] };
    // SAFETY: plain calls on a window of ours with structs of ours.
    unsafe {
        if r.right - r.left < 200 || r.bottom - r.top < 150 || MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_null() {
            return;
        }
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        GetWindowPlacement(hwnd, &mut wp);
        wp.rcNormalPosition = r;
        wp.showCmd = match (show, maximized) {
            (false, _) => SW_HIDE as u32,
            (true, true) => SW_SHOWMAXIMIZED as u32,
            (true, false) => SW_SHOWNORMAL as u32,
        };
        // Hidden but maximised: shown later, it comes back maximised.
        wp.flags = if maximized { WPF_RESTORETOMAXIMIZED } else { 0 };
        SetWindowPlacement(hwnd, &wp);
    }
}

/// Close for real, even when resident.
pub fn destroy(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe { DestroyWindow(hwnd) };
}

pub fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a window of ours.
    unsafe { GetClientRect(hwnd, &mut r) };
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

pub fn dpi(hwnd: HWND) -> u32 {
    // SAFETY: a window of ours.
    unsafe { GetDpiForWindow(hwnd).max(96) }
}

pub fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: a window of ours.
    unsafe { IsWindowVisible(hwnd) != 0 }
}

pub fn is_maximized(hwnd: HWND) -> bool {
    // SAFETY: a window of ours.
    unsafe { IsZoomed(hwnd) != 0 }
}

pub fn minimize(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
}

pub fn toggle_maximize(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe { ShowWindow(hwnd, if is_maximized(hwnd) { SW_RESTORE } else { SW_MAXIMIZE }) };
}

pub fn close(hwnd: HWND) {
    // SAFETY: a window of ours.
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
}

pub fn set_title(hwnd: HWND, t: &str) {
    let w = wide(t);
    // SAFETY: a window of ours, the string lives through the call.
    unsafe { SetWindowTextW(hwnd, w.as_ptr()) };
}

/// The size of the monitor the window is on.
pub fn monitor_size(hwnd: HWND) -> (u32, u32) {
    // SAFETY: plain queries into a struct of ours.
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
        let r = mi.rcMonitor;
        ((r.right - r.left).max(640) as u32, (r.bottom - r.top).max(480) as u32)
    }
}

/// Borderless fullscreen over the window's monitor, and back to where it was (Raymond Chen's
/// recipe: the placement is saved, the overlapped style taken off and put back).
pub struct Fullscreen {
    saved: WINDOWPLACEMENT,
    pub on: bool,
}

impl Fullscreen {
    pub fn new() -> Self {
        // SAFETY: a plain struct.
        let mut saved: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        saved.length = size_of::<WINDOWPLACEMENT>() as u32;
        Fullscreen { saved, on: false }
    }

    pub fn set(&mut self, hwnd: HWND, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        // SAFETY: a window of ours; structs are ours.
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            if on {
                GetWindowPlacement(hwnd, &mut self.saved);
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = size_of::<MONITORINFO>() as u32;
                GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);
                let r = mi.rcMonitor;
                SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !WS_OVERLAPPEDWINDOW | WS_POPUP) as isize);
                SetWindowPos(hwnd, HWND_TOP, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
            } else {
                SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !WS_POPUP | WS_OVERLAPPEDWINDOW) as isize);
                SetWindowPlacement(hwnd, &self.saved);
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
            }
        }
    }
}

fn lo(l: LPARAM) -> i32 {
    (l & 0xffff) as u16 as i16 as i32
}
fn hi(l: LPARAM) -> i32 {
    ((l >> 16) & 0xffff) as u16 as i16 as i32
}

/// Where a point is: an edge to resize by, the caption, or the client area. The edges are
/// inside the client area, a few pixels wide: there is no frame left to hold them.
unsafe fn hit_test(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // SAFETY: queries on a window of ours.
    unsafe {
        let (cap, buttons) = CAPTION.get();
        if cap == 0 {
            return HTCLIENT as LRESULT;
        }
        let mut p = POINT { x: lo(lparam), y: hi(lparam) };
        ScreenToClient(hwnd, &mut p);
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetClientRect(hwnd, &mut r);
        if IsZoomed(hwnd) == 0 {
            let band = (6 * GetDpiForWindow(hwnd) as i32 / 96).max(4);
            let (l, rt, t, b) = (p.x < band, p.x >= r.right - band, p.y < band, p.y >= r.bottom - band);
            match (l, rt, t, b) {
                (true, _, true, _) => return HTTOPLEFT as LRESULT,
                (_, true, true, _) => return HTTOPRIGHT as LRESULT,
                (true, _, _, true) => return HTBOTTOMLEFT as LRESULT,
                (_, true, _, true) => return HTBOTTOMRIGHT as LRESULT,
                (true, _, _, _) => return HTLEFT as LRESULT,
                (_, true, _, _) => return HTRIGHT as LRESULT,
                (_, _, true, _) => return HTTOP as LRESULT,
                (_, _, _, true) => return HTBOTTOM as LRESULT,
                _ => {}
            }
        }
        if p.y < cap && p.x < r.right - buttons {
            return HTCAPTION as LRESULT;
        }
        HTCLIENT as LRESULT
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: every call below is on this window with the message's own arguments.
    unsafe {
        match msg {
            WM_CLOSE => {
                post(Ev::Close);
                0
            }
            WM_QUIT_HOST => {
                post(Ev::Quit);
                0
            }
            WM_COPYDATA => {
                let cds = &*(lparam as *const windows_sys::Win32::System::DataExchange::COPYDATASTRUCT);
                if cds.dwData != COPYDATA_OPEN {
                    return 0;
                }
                let units = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                let path = (!units.is_empty()).then(|| PathBuf::from(OsString::from_wide(units)));
                post(Ev::Open(path));
                1
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                BeginPaint(hwnd, &mut ps);
                EndPaint(hwnd, &ps);
                post(Ev::Paint);
                0
            }
            WM_ERASEBKGND if READY.get() => 1,
            // NO FRAME: the client area is the whole window. Maximised, the system still places
            // the window past the screen by its (now invisible) frame, and that much is taken
            // back, or the caption would sit off the monitor.
            WM_NCCALCSIZE if wparam != 0 => {
                if IsZoomed(hwnd) != 0 {
                    let p = &mut *(lparam as *mut NCCALCSIZE_PARAMS);
                    let dpi = GetDpiForWindow(hwnd);
                    let f = GetSystemMetricsForDpi(SM_CXFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                    let g = GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                    let r = &mut p.rgrc[0];
                    r.left += f;
                    r.right -= f;
                    r.top += g;
                    r.bottom -= g;
                }
                0
            }
            WM_NCHITTEST => hit_test(hwnd, lparam),
            WM_SIZE => {
                let (w, h) = ((lparam & 0xffff) as u32, ((lparam >> 16) & 0xffff) as u32);
                if wparam as u32 != SIZE_MINIMIZED && w > 0 && h > 0 {
                    post(Ev::Resize(w, h));
                }
                0
            }
            WM_GETMINMAXINFO => {
                let mm = &mut *(lparam as *mut MINMAXINFO);
                let k = GetDpiForWindow(hwnd).max(96) as i32;
                mm.ptMinTrackSize = POINT { x: 420 * k / 96, y: 300 * k / 96 };
                0
            }
            WM_DPICHANGED => {
                let r = &*(lparam as *const RECT);
                SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                post(Ev::Dpi((wparam & 0xffff) as u32));
                0
            }
            WM_SETCURSOR if (lparam & 0xffff) as u32 == HTCLIENT => {
                let id = match CURSOR.get() {
                    0 => IDC_ARROW,
                    c => c as windows_sys::core::PCWSTR,
                };
                SetCursor(LoadCursorW(std::ptr::null_mut(), id));
                1
            }
            WM_MOUSEMOVE => {
                if !TRACKING.get() {
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut t);
                    TRACKING.set(true);
                }
                post(Ev::MouseMove(lo(lparam), hi(lparam)));
                0
            }
            WM_MOUSELEAVE => {
                TRACKING.set(false);
                post(Ev::MouseLeave);
                0
            }
            WM_LBUTTONDBLCLK => {
                post(Ev::DoubleClick(lo(lparam), hi(lparam)));
                0
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let (b, down) = match msg {
                    WM_LBUTTONDOWN => (Btn::Left, true),
                    WM_LBUTTONUP => (Btn::Left, false),
                    WM_RBUTTONDOWN => (Btn::Right, true),
                    WM_RBUTTONUP => (Btn::Right, false),
                    WM_MBUTTONDOWN => (Btn::Middle, true),
                    WM_MBUTTONUP => (Btn::Middle, false),
                    _ => {
                        let back = (wparam >> 16) & 0xffff == 1;
                        (if back { Btn::Back } else { Btn::Forward }, msg == WM_XBUTTONDOWN)
                    }
                };
                // The press captures the mouse: a pan keeps following the hand off the window.
                if b == Btn::Left {
                    if down {
                        SetCapture(hwnd);
                    } else {
                        ReleaseCapture();
                    }
                }
                post(Ev::Button { b, down, x: lo(lparam), y: hi(lparam) });
                if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) { 1 } else { 0 }
            }
            WM_MOUSEWHEEL => {
                let mut p = POINT { x: lo(lparam), y: hi(lparam) };
                ScreenToClient(hwnd, &mut p);
                let notches = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / WHEEL_DELTA as f32;
                post(Ev::Wheel { notches, x: p.x, y: p.y });
                0
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
                let repeat = down && (lparam >> 30) & 1 == 1;
                post(Ev::Key { vk: wparam as u16, down, repeat });
                // ALT+F4 AND ALT+SPACE ARE THE SYSTEM'S; other Alt keys would beep.
                if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                0
            }
            // A tap of Alt or F10 would move the keyboard into a menu we do not have.
            WM_SYSCOMMAND if (wparam & 0xfff0) as u32 == SC_KEYMENU && (lparam >> 16) <= 0 => 0,
            WM_SYSCHAR => 0,
            WM_DROPFILES => {
                let drop = wparam as HDROP;
                let n = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
                for i in 0..n.min(1) {
                    let len = DragQueryFileW(drop, i, std::ptr::null_mut(), 0);
                    let mut buf = vec![0u16; len as usize + 1];
                    DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
                    buf.truncate(len as usize);
                    // FROM WIDE, NOT THROUGH A STRING: a file name is not always valid UTF-16.
                    post(Ev::Drop(OsString::from_wide(&buf).into()));
                }
                DragFinish(drop);
                0
            }
            WM_ADD_TAB => {
                taskbar_tab(hwnd, true);
                0
            }
            WM_TRAY => {
                match (lparam & 0xffff) as u32 {
                    WM_LBUTTONUP => post(Ev::Tray(TrayCmd::Open)),
                    WM_RBUTTONUP => {
                        if let Some(c) = tray_menu(hwnd) {
                            post(Ev::Tray(c));
                        }
                    }
                    _ => {}
                }
                0
            }
            // Explorer restarted: the notification area is new and empty.
            m if m == taskbar_created() && m != 0 => {
                tray_add(hwnd);
                0
            }
            WM_TIMER => {
                KillTimer(hwnd, wparam);
                post(Ev::Timer(wparam));
                0
            }
            crate::loader::WM_LOADED => {
                post(Ev::Loaded);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
