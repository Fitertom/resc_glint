//! An image from the clipboard, as a file the viewer can open like any other.
//!
//! **Through a file, not straight to the GPU**: then flipping, zoom, delete-to-bin and the
//! caption work the same as for everything else, with no second path through the viewer. The
//! file lives alone in `%TEMP%\Glint\clipboard`, so its folder has nothing else to flip to.

use std::path::PathBuf;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

const CF_DIB: u32 = 8;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

/// What is on the clipboard, as a path to open: a copied image file itself, or the copied
/// pixels written out. `None` when there is no image.
pub fn image(owner: isize) -> Option<PathBuf> {
    // SAFETY: the clipboard is opened and always closed below; handles are only read while
    // locked.
    unsafe {
        // Another program may hold it for a moment (a clipboard manager reading the change).
        let mut open = false;
        for _ in 0..5 {
            if OpenClipboard(owner as _) != 0 {
                open = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !open {
            return None;
        }
        let out = read();
        CloseClipboard();
        out
    }
}

unsafe fn read() -> Option<PathBuf> {
    // SAFETY: called with the clipboard open.
    unsafe {
        // Files copied in Explorer: the first one that is an image, opened in its own folder.
        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            let drop = GetClipboardData(CF_HDROP) as HDROP;
            if !drop.is_null() {
                let n = DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0);
                for i in 0..n {
                    let len = DragQueryFileW(drop, i, std::ptr::null_mut(), 0);
                    let mut buf = vec![0u16; len as usize + 1];
                    DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
                    buf.truncate(len as usize);
                    let p = PathBuf::from(<std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&buf));
                    if crate::files::is_image(&p) {
                        return Some(p);
                    }
                }
            }
        }
        // Pixels. PNG first: screenshots and browsers put it there, with alpha and small.
        let png = RegisterClipboardFormatW(crate::win::wide("PNG").as_ptr());
        if IsClipboardFormatAvailable(png) != 0
            && let Some(bytes) = global_bytes(GetClipboardData(png))
        {
            return save(&bytes, "png");
        }
        for fmt in [CF_DIBV5, CF_DIB] {
            if IsClipboardFormatAvailable(fmt) != 0
                && let Some(dib) = global_bytes(GetClipboardData(fmt))
                && let Some(bmp) = bmp_file(&dib)
            {
                return save(&bmp, "bmp");
            }
        }
        None
    }
}

unsafe fn global_bytes(h: windows_sys::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    if h.is_null() {
        return None;
    }
    // SAFETY: a clipboard HGLOBAL, locked for the copy.
    unsafe {
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, GlobalSize(h)).to_vec();
        GlobalUnlock(h);
        Some(v)
    }
}

/// A packed DIB (header, masks, palette, pixels) with the 14-byte file header in front: the
/// clipboard's bitmap as a .bmp that WIC opens.
fn bmp_file(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |o: usize| dib.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header = u32_at(0)? as usize;
    let bpp = u16::from_le_bytes([*dib.get(14)?, *dib.get(15)?]) as u32;
    let compression = u32_at(16)?;
    let used = u32_at(32)?;
    // Three colour masks follow a plain 40-byte header when the pixels are bit fields.
    let masks = if header == 40 && compression == 3 { 12 } else { 0 };
    let palette = if used > 0 { used } else if bpp <= 8 { 1 << bpp } else { 0 } as usize * 4;
    let offset = 14 + header + masks + palette;
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(offset as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Some(out)
}

/// Write the pixels as the only file of the clipboard folder, under a new name each time: the
/// viewer caches by path, and the same name would show the previous clipboard.
fn save(bytes: &[u8], ext: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("Glint").join("clipboard");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let path = dir.join(format!("clipboard-{stamp}.{ext}"));
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}
