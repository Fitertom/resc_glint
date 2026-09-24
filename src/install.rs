//! Installing like a normal Windows program, per user and without admin rights: the exe goes
//! to `%LOCALAPPDATA%\Programs\Glint`, a shortcut to the Start menu, and an entry to Settings ▸
//! Apps to uninstall from. There is no installer: the viewer copies itself.

use crate::com::{Com, check};
use crate::win::wide;
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows_sys::core::GUID;

const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_c000_000000000046);
const IID_SHELL_LINK_W: GUID = GUID::from_u128(0x000214f9_0000_0000_c000_000000000046);
const IID_PERSIST_FILE: GUID = GUID::from_u128(0x0000010b_0000_0000_c000_000000000046);
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Glint";

pub fn dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Programs").join("Glint")
}

pub fn target() -> PathBuf {
    dir().join("glint.exe")
}

fn same(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// This process is the installed copy.
pub fn is_installed_copy() -> bool {
    std::env::current_exe().is_ok_and(|e| same(&e, &target()))
}

fn shortcut_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join(r"Microsoft\Windows\Start Menu\Programs\Glint.lnk")
}

/// Copy the running exe (and its settings) into place, add the shortcut and the uninstall
/// entry. Returns the installed exe.
pub fn install() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = dir();
    let dst = target();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !same(&exe, &dst) {
        if std::fs::copy(&exe, &dst).is_err() {
            // AN OLDER COPY IS RUNNING and holds its file. Windows lets a running exe be renamed
            // though, and the old one is swept away on the next install.
            let old = dir.join("glint.old.exe");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&dst, &old).map_err(|e| format!("{}: {e}", dst.display()))?;
            std::fs::copy(&exe, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        }
        let ini = exe.with_file_name("glint.ini");
        if ini.exists() && !dir.join("glint.ini").exists() {
            let _ = std::fs::copy(&ini, dir.join("glint.ini"));
        }
    }
    crate::config::store_at(&dir.join("glint.ini"), "setup_done", "true");
    shortcut(&dst)?;
    let d = dst.display().to_string();
    let set = crate::assoc::set;
    set(UNINSTALL, Some("DisplayName"), "Glint")?;
    set(UNINSTALL, Some("DisplayIcon"), &format!("\"{d}\",0"))?;
    set(UNINSTALL, Some("DisplayVersion"), env!("CARGO_PKG_VERSION"))?;
    set(UNINSTALL, Some("Publisher"), "Glint")?;
    set(UNINSTALL, Some("InstallLocation"), &dir.display().to_string())?;
    set(UNINSTALL, Some("UninstallString"), &format!("\"{d}\" --uninstall"))?;
    set(UNINSTALL, Some("QuietUninstallString"), &format!("\"{d}\" --uninstall"))?;
    let kb = std::fs::metadata(&dst).map_or(0, |m| m.len() / 1024) as u32;
    crate::assoc::set_dword(UNINSTALL, "EstimatedSize", kb)?;
    crate::assoc::set_dword(UNINSTALL, "NoModify", 1)?;
    crate::assoc::set_dword(UNINSTALL, "NoRepair", 1)?;
    Ok(dst)
}

/// The Start menu shortcut, through the shell's own IShellLink.
fn shortcut(exe: &Path) -> Result<(), String> {
    // SAFETY: COM on this thread, objects released by `Com`; slots as in shobjidl.h / objidl.h:
    // IShellLinkW SetDescription 7, SetWorkingDirectory 9, SetIconLocation 17, SetPath 20;
    // IPersistFile Save 6.
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut p = std::ptr::null_mut();
        check(CoCreateInstance(&CLSID_SHELL_LINK, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_SHELL_LINK_W, &mut p), "ShellLink")?;
        let link = Com(p);
        type SetStr = unsafe extern "system" fn(*mut std::ffi::c_void, *const u16) -> i32;
        let path = wide(&exe.display().to_string());
        let dir = wide(&exe.parent().unwrap_or(exe).display().to_string());
        link.slot::<SetStr>(20)(link.raw(), path.as_ptr());
        link.slot::<SetStr>(9)(link.raw(), dir.as_ptr());
        link.slot::<SetStr>(7)(link.raw(), wide("Fast image viewer").as_ptr());
        link.slot::<unsafe extern "system" fn(*mut std::ffi::c_void, *const u16, i32) -> i32>(17)(link.raw(), path.as_ptr(), 0);
        let file = link.query(&IID_PERSIST_FILE).ok_or("IPersistFile")?;
        let at = shortcut_path();
        if let Some(parent) = at.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let at = wide(&at.display().to_string());
        check(file.slot::<unsafe extern "system" fn(*mut std::ffi::c_void, *const u16, i32) -> i32>(6)(file.raw(), at.as_ptr(), 1), "shortcut")
    }
}

/// `--uninstall`, as Settings ▸ Apps runs it: stop the resident copy, take everything back out
/// of Windows, and delete the folder once this process has let go of its exe.
pub fn uninstall() -> Result<(), String> {
    if let Some(h) = crate::win::find_host() {
        // SAFETY: posting to a window handle.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(h, crate::win::WM_QUIT_HOST, 0, 0) };
        for _ in 0..50 {
            if crate::win::find_host().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let exe = target();
    crate::assoc::set_autostart(false, &exe)?;
    crate::assoc::set_default(false, &exe)?;
    let _ = std::fs::remove_file(shortcut_path());
    // SAFETY: a NUL-terminated string living through the call.
    unsafe { windows_sys::Win32::System::Registry::RegDeleteTreeW(windows_sys::Win32::System::Registry::HKEY_CURRENT_USER, wide(UNINSTALL).as_ptr()) };
    let dir = dir();
    // Only ever our own folder: the path is built here, never taken from outside.
    if dir.ends_with(r"Programs\Glint") && dir.exists() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
        let _ = std::process::Command::new("cmd").args(["/c", &script]).creation_flags(CREATE_NO_WINDOW).spawn();
    }
    Ok(())
}
