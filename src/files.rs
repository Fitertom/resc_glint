//! The folder being browsed: which files are images, in Explorer's order; the open dialog and
//! the recycle bin.

use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, SHFILEOPSTRUCTW, SHFileOperationW, StrCmpLogicalW};

/// What WIC can open on a stock Windows, plus what the Store codecs add (webp, heic, avif, jxl)
/// — those fail with a message where the codec is missing.
pub const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "bmp", "dib", "gif", "tif", "tiff", "ico", "jxr", "wdp", "hdp", "dds", "webp", "heic",
    "heif", "avif", "jxl",
];

pub fn is_image(p: &Path) -> bool {
    p.extension().and_then(OsStr::to_str).is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Images of `dir`, sorted as Explorer sorts them: `IMG_2` before `IMG_10`.
pub fn list(dir: &Path) -> Vec<Arc<Path>> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<(Vec<u16>, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| is_image(p))
        .map(|p| (wide(p.file_name().unwrap_or_default()), p))
        .collect();
    // SAFETY: both strings are NUL-terminated and live through the call.
    v.sort_by(|a, b| unsafe { StrCmpLogicalW(a.0.as_ptr(), b.0.as_ptr()) }.cmp(&0));
    v.into_iter().map(|(_, p)| Arc::from(p)).collect()
}

/// Same file name, ignoring case: the path from the command line and the one from the folder
/// listing can differ in case and in how the folder is spelled.
pub fn same_name(a: &Path, b: &Path) -> bool {
    match (a.file_name(), b.file_name()) {
        (Some(x), Some(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

pub fn open_dialog(owner: isize) -> Option<PathBuf> {
    let mut filter: Vec<u16> = Vec::new();
    let pattern: String = EXTENSIONS.iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";");
    for s in ["Images", &pattern, "All files", "*.*"] {
        filter.extend(s.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    let mut buf = vec![0u16; 32768];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner as _;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    // SAFETY: every pointer in `ofn` lives through the call.
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

/// To the recycle bin, with the system's own confirmation. True if the file is gone.
pub fn recycle(owner: isize, p: &Path) -> bool {
    let mut from = wide(p.as_os_str());
    from.push(0); // the list is double-NUL terminated
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.hwnd = owner as _;
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = FOF_ALLOWUNDO as u16;
    // SAFETY: `from` lives through the call.
    unsafe { SHFileOperationW(&mut op) };
    !p.exists()
}
