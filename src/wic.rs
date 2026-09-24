//! Decoding through WIC, the codecs Windows already has: nothing added to the exe, and every
//! format the system knows (webp/heic/avif too, where the Store extensions are installed).
//!
//! windows-sys has no COM interfaces, only functions, so the few methods used are called
//! through their vtable slots by hand. Slot numbers are from the SDK's `wincodec.h`.

use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows_sys::core::GUID;

windows_link::link!("ole32.dll" "system" fn PropVariantClear(pvar: *mut c_void) -> i32);

const CLSID_WIC_FACTORY: GUID = GUID::from_u128(0xcacaf262_9370_4615_a13b_9f5539da4c0a);
const IID_WIC_FACTORY: GUID = GUID::from_u128(0xec5ec8a9_c395_4314_9c77_54d7a935ff70);
const IID_SOURCE_TRANSFORM: GUID = GUID::from_u128(0x3b16811b_6a43_4ec9_b713_3d5a0c13b940);
const FMT_PBGRA: GUID = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc910);
const FMT_BGRA: GUID = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc90f);
const FMT_BGR32: GUID = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc90e);
const FMT_BGR24: GUID = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc90c);
const FMT_GRAY8: GUID = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc908);

const GENERIC_READ: u32 = 0x8000_0000;
const METADATA_ON_DEMAND: u32 = 0;
const INTERP_FANT: u32 = 3;
const VT_UI2: u16 = 18;

/// Rows copied at a time from a full decode: between bands the job checks it is still wanted,
/// so flipping past an image stops its decode instead of letting it finish for nothing.
const BAND: u32 = 256;

/// Decoded pixels: premultiplied BGRA, top-down, tightly packed.
pub struct Img {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
    /// The image's real size: a preview is decoded smaller, and zoom is measured against this.
    pub full_w: u32,
    pub full_h: u32,
    /// EXIF orientation, 1..=8.
    pub orient: u8,
    /// Nothing sharper will come: full size, or as much as the GPU can hold.
    pub full: bool,
    /// A stand-in from the thumbnail cache or the file's EXIF thumbnail: shown for the few
    /// milliseconds until the real pixels come.
    pub thumb: bool,
    /// Unique per decode: what a GPU slot remembers instead of holding the pixels alive.
    pub id: u64,
}

impl Img {
    /// How good these pixels are: thumbnail 0, preview 1, full 2.
    pub fn rank(&self) -> u8 {
        if self.full { 2 } else if self.thumb { 0 } else { 1 }
    }
}

fn next_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

use crate::com::{Com, Out, check};

pub struct Wic {
    factory: Com,
}

impl Wic {
    /// Per thread: COM is joined here as multithreaded, and the factory is free-threaded.
    pub fn new() -> Option<Wic> {
        // SAFETY: plain COM start-up; the factory pointer is written by the call.
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
            let mut f = std::ptr::null_mut();
            let hr = CoCreateInstance(&CLSID_WIC_FACTORY, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_WIC_FACTORY, &mut f);
            if hr < 0 || f.is_null() { None } else { Some(Wic { factory: Com(f) }) }
        }
    }

    /// Decode `path`. With `target` (a screen size) a JPEG is decoded scaled down in the codec
    /// itself — 1/2, 1/4, 1/8 of the DCT — as long as it still covers the screen at fit;
    /// several times faster than a full decode, which is what makes flipping quick.
    /// `max_dim` is the GPU's texture limit. `wanted` is asked between bands; false aborts.
    pub fn decode(&self, path: &Path, target: Option<(u32, u32)>, max_dim: u32, wanted: &dyn Fn() -> bool) -> Result<Img, String> {
        // SAFETY: every call below goes through the slot `wincodec.h` gives it, with pointers
        // that live through the call; objects are released by `Com`'s drop.
        unsafe {
            let wpath = crate::files::wide(path.as_os_str());
            let mut dec = std::ptr::null_mut();
            let create: unsafe extern "system" fn(*mut c_void, *const u16, *const GUID, u32, u32, Out) -> i32 = self.factory.slot(3);
            check(create(self.factory.raw(), wpath.as_ptr(), std::ptr::null(), GENERIC_READ, METADATA_ON_DEMAND, &mut dec), "open")?;
            let dec = Com(dec);
            let mut frame = std::ptr::null_mut();
            let get_frame: unsafe extern "system" fn(*mut c_void, u32, Out) -> i32 = dec.slot(13);
            check(get_frame(dec.raw(), 0, &mut frame), "frame")?;
            let frame = Com(frame);
            let (fw, fh) = size_of_source(&frame)?;
            if fw == 0 || fh == 0 {
                return Err("empty image".into());
            }
            let orient = orientation(&frame);

            if let Some((tw, th)) = target {
                // The target is a screen; the image lies across it turned by its orientation.
                let (tw, th) = if orient >= 5 { (th, tw) } else { (tw, th) };
                let fit = (tw as f64 / fw as f64).min(th as f64 / fh as f64);
                if let Some(img) = scaled(&frame, fw, fh, fit, orient)? {
                    return Ok(img);
                }
            }

            // Full decode: to premultiplied BGRA, shrunk only if the GPU could not hold it.
            let mut src: &Com = &frame;
            let (mut w, mut h) = (fw, fh);
            let scaler;
            if fw.max(fh) > max_dim {
                let k = max_dim as f64 / fw.max(fh) as f64;
                (w, h) = (((fw as f64 * k) as u32).max(1), ((fh as f64 * k) as u32).max(1));
                let mut s = std::ptr::null_mut();
                let make: unsafe extern "system" fn(*mut c_void, Out) -> i32 = self.factory.slot(11);
                check(make(self.factory.raw(), &mut s), "scaler")?;
                scaler = Com(s);
                let init: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> i32 = scaler.slot(8);
                check(init(scaler.raw(), frame.raw(), w, h, INTERP_FANT), "scale")?;
                src = &scaler;
            }
            let mut c = std::ptr::null_mut();
            let make: unsafe extern "system" fn(*mut c_void, Out) -> i32 = self.factory.slot(10);
            check(make(self.factory.raw(), &mut c), "converter")?;
            let conv = Com(c);
            let init: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, u32, *mut c_void, f64, u32) -> i32 = conv.slot(8);
            check(init(conv.raw(), src.raw(), &FMT_PBGRA, 0, std::ptr::null_mut(), 0.0, 0), "convert")?;

            let stride = w as usize * 4;
            let mut px: Vec<u8> = Vec::with_capacity(stride * h as usize);
            let copy: unsafe extern "system" fn(*mut c_void, *const [i32; 4], u32, u32, *mut u8) -> i32 = conv.slot(7);
            let mut y = 0;
            while y < h {
                if !wanted() {
                    return Err("cancelled".into());
                }
                let n = BAND.min(h - y);
                let rect = [0, y as i32, w as i32, n as i32];
                let dst = px.as_mut_ptr().add(y as usize * stride);
                check(copy(conv.raw(), &rect, stride as u32, (stride * n as usize) as u32, dst), "decode")?;
                y += n;
            }
            px.set_len(stride * h as usize); // every row was written by the codec
            Ok(Img { w, h, px, full_w: fw, full_h: fh, orient, full: true, thumb: false, id: next_id() })
        }
    }
}

impl Wic {
    /// Load the shell's thumbnail machinery now: its first use in a process costs ~15 ms of DLL
    /// loading, which would otherwise land on the first photo opened.
    pub fn warm(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = shell_thumbnail(&crate::files::wide(exe.as_os_str()));
        }
    }

    /// A small encoded picture held in memory (a cache entry) to premultiplied BGRA.
    fn decode_bytes(&self, data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
        // SAFETY: vtable slots as in wincodec.h (factory CreateStream 14, CreateDecoderFromStream
        // 4; IWICStream::InitializeFromMemory 16); `data` outlives the stream and decoder.
        unsafe {
            let mut st = std::ptr::null_mut();
            let make: unsafe extern "system" fn(*mut c_void, Out) -> i32 = self.factory.slot(14);
            check(make(self.factory.raw(), &mut st), "stream")?;
            let stream = Com(st);
            let init: unsafe extern "system" fn(*mut c_void, *const u8, u32) -> i32 = stream.slot(16);
            check(init(stream.raw(), data.as_ptr(), data.len() as u32), "memory")?;
            let mut dec = std::ptr::null_mut();
            let create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, u32, Out) -> i32 = self.factory.slot(4);
            check(create(self.factory.raw(), stream.raw(), std::ptr::null(), METADATA_ON_DEMAND, &mut dec), "decoder")?;
            let dec = Com(dec);
            let mut frame = std::ptr::null_mut();
            let get_frame: unsafe extern "system" fn(*mut c_void, u32, Out) -> i32 = dec.slot(13);
            check(get_frame(dec.raw(), 0, &mut frame), "frame")?;
            let frame = Com(frame);
            let (w, h) = size_of_source(&frame)?;
            if w == 0 || h == 0 || w > 4096 || h > 4096 {
                return Err("odd size".into());
            }
            let mut c = std::ptr::null_mut();
            let make: unsafe extern "system" fn(*mut c_void, Out) -> i32 = self.factory.slot(10);
            check(make(self.factory.raw(), &mut c), "converter")?;
            let conv = Com(c);
            let init: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, u32, *mut c_void, f64, u32) -> i32 = conv.slot(8);
            check(init(conv.raw(), frame.raw(), &FMT_PBGRA, 0, std::ptr::null_mut(), 0.0, 0), "convert")?;
            let mut px = vec![0u8; (w * h * 4) as usize];
            let copy: unsafe extern "system" fn(*mut c_void, *const [i32; 4], u32, u32, *mut u8) -> i32 = conv.slot(7);
            check(copy(conv.raw(), std::ptr::null(), w * 4, px.len() as u32, px.as_mut_ptr()), "pixels")?;
            if px.chunks_exact(4).all(|p| p[3] == 0) {
                px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
            }
            Ok((w, h, px))
        }
    }

    /// A stand-in in a few milliseconds, never a decode of the image itself. First the shell's
    /// thumbnail cache — anything Explorer has shown as a thumbnail is there, PNG and HEIC
    /// too — then the small JPEG a camera or phone embeds in its EXIF.
    pub fn thumbnail(&self, path: &Path) -> Result<Img, String> {
        crate::trace("      [thumb] start");
        // SAFETY: vtable slots as in wincodec.h; objects released by `Com`.
        unsafe {
            let wpath = crate::files::wide(path.as_os_str());
            let mut dec = std::ptr::null_mut();
            let create: unsafe extern "system" fn(*mut c_void, *const u16, *const GUID, u32, u32, Out) -> i32 = self.factory.slot(3);
            check(create(self.factory.raw(), wpath.as_ptr(), std::ptr::null(), GENERIC_READ, METADATA_ON_DEMAND, &mut dec), "open")?;
            let dec = Com(dec);
            let mut frame = std::ptr::null_mut();
            let get_frame: unsafe extern "system" fn(*mut c_void, u32, Out) -> i32 = dec.slot(13);
            check(get_frame(dec.raw(), 0, &mut frame), "frame")?;
            let frame = Com(frame);
            let (fw, fh) = size_of_source(&frame)?;
            let orient = orientation(&frame);
            crate::trace("      [thumb] header read");
            // The shell's thumbnail is already turned upright: it stands for the image as
            // displayed, so its size is the displayed one and it has no orientation of its own.
            // Explorer's cache files read directly first (tens of microseconds), the shell's
            // own lookup (~7 ms) only when that finds nothing.
            let found = match crate::thumbcache::lookup(path).and_then(|data| self.decode_bytes(&data).ok()) {
                Some(t) => {
                    crate::trace("      [thumb] cache file hit");
                    Some(t)
                }
                None => {
                    let t = shell_thumbnail(&wpath);
                    crate::trace("      [thumb] shell lookup");
                    t
                }
            };
            if let Some((w, h, px)) = found {
                let (dw, dh) = if orient >= 5 { (fh, fw) } else { (fw, fh) };
                return Ok(Img { w, h, px, full_w: dw, full_h: dh, orient: 1, full: false, thumb: true, id: next_id() });
            }
            // IWICBitmapFrameDecode::GetThumbnail (10): the EXIF one, stored as the image is.
            let mut t = std::ptr::null_mut();
            let get: unsafe extern "system" fn(*mut c_void, Out) -> i32 = frame.slot(10);
            check(get(frame.raw(), &mut t), "no thumbnail")?;
            let t = Com(t);
            let (tw, th) = size_of_source(&t)?;
            let mut c = std::ptr::null_mut();
            let make: unsafe extern "system" fn(*mut c_void, Out) -> i32 = self.factory.slot(10);
            check(make(self.factory.raw(), &mut c), "converter")?;
            let conv = Com(c);
            let init: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, u32, *mut c_void, f64, u32) -> i32 = conv.slot(8);
            check(init(conv.raw(), t.raw(), &FMT_PBGRA, 0, std::ptr::null_mut(), 0.0, 0), "convert")?;
            let mut px = vec![0u8; (tw * th * 4) as usize];
            let copy: unsafe extern "system" fn(*mut c_void, *const [i32; 4], u32, u32, *mut u8) -> i32 = conv.slot(7);
            check(copy(conv.raw(), std::ptr::null(), tw * 4, px.len() as u32, px.as_mut_ptr()), "thumbnail")?;
            // A 160×120 box round a 3:2 photo carries black bars: cut back to the photo's shape.
            let (w, h, px) = crop_to_aspect(px, tw, th, fw as f64 / fh as f64);
            Ok(Img { w, h, px, full_w: fw, full_h: fh, orient, full: false, thumb: true, id: next_id() })
        }
    }
}

/// The largest thumbnail the shell has cached for this file, without making one: making one
/// is a full decode, which is what this is here to avoid.
fn shell_thumbnail(wpath: &[u16]) -> Option<(u32, u32, Vec<u8>)> {
    use windows_sys::Win32::Graphics::Gdi::*;
    const IID_IMAGE_FACTORY: GUID = GUID::from_u128(0xbcc18b79_ba16_442f_80c4_8a59c30c463b);
    const BIGGER_OK: u32 = 0x1;
    const THUMBNAIL_ONLY: u32 = 0x8;
    const IN_CACHE_ONLY: u32 = 0x10;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        cx: i32,
        cy: i32,
    }
    // SAFETY: IShellItemImageFactory::GetImage is slot 3; the bitmap is ours to delete.
    unsafe {
        let mut f = std::ptr::null_mut();
        let hr = windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName(wpath.as_ptr(), std::ptr::null_mut(), &IID_IMAGE_FACTORY, &mut f);
        if hr < 0 || f.is_null() {
            return None;
        }
        let factory = Com(f);
        crate::trace("        [shell] item parsed");
        let get: unsafe extern "system" fn(*mut c_void, Size, u32, *mut HBITMAP) -> i32 = factory.slot(3);
        let mut hbm: HBITMAP = std::ptr::null_mut();
        let flags = BIGGER_OK | THUMBNAIL_ONLY | IN_CACHE_ONLY;
        // 256 first: Explorer's own size, so the likeliest hit; a miss costs a lookup.
        if [256, 1024].into_iter().all(|side| get(factory.raw(), Size { cx: side, cy: side }, flags, &mut hbm) < 0 || hbm.is_null()) {
            return None;
        }
        crate::trace("        [shell] cache answered");
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(hbm, size_of::<BITMAP>() as i32, (&mut bm as *mut BITMAP).cast());
        let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.unsigned_abs());
        let mut px = vec![0u8; (w * h * 4) as usize];
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w as i32;
        bmi.bmiHeader.biHeight = -(h as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let dc = GetDC(std::ptr::null_mut());
        let rows = GetDIBits(dc, hbm, 0, h, px.as_mut_ptr().cast(), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(std::ptr::null_mut(), dc);
        DeleteObject(hbm);
        if rows == 0 || w == 0 || h == 0 {
            return None;
        }
        // An opaque image's thumbnail can come with alpha left at zero.
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        }
        Some((w, h, px))
    }
}

/// The centre of a `w × h` picture cut to `aspect` (width / height), if it is off by more
/// than a few percent.
fn crop_to_aspect(px: Vec<u8>, w: u32, h: u32, aspect: f64) -> (u32, u32, Vec<u8>) {
    let have = w as f64 / h as f64;
    if (have / aspect - 1.0).abs() < 0.03 {
        return (w, h, px);
    }
    let (cw, ch) = if have > aspect { (((h as f64) * aspect).round() as u32, h) } else { (w, ((w as f64) / aspect).round() as u32) };
    let (x0, y0) = ((w - cw.min(w)) / 2, (h - ch.min(h)) / 2);
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y0 + ch.min(h) {
        let row = (y * w + x0) as usize * 4;
        out.extend_from_slice(&px[row..row + cw.min(w) as usize * 4]);
    }
    (cw.min(w), ch.min(h), out)
}

unsafe fn size_of_source(src: &Com) -> Result<(u32, u32), String> {
    let (mut w, mut h) = (0, 0);
    // SAFETY: IWICBitmapSource::GetSize, slot 3.
    unsafe {
        let get: unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32) -> i32 = src.slot(3);
        check(get(src.raw(), &mut w, &mut h), "size")?;
    }
    Ok((w, h))
}

/// EXIF orientation through the photo metadata policy, which reads it from JPEG, TIFF, HEIC
/// alike. 1 when there is none.
unsafe fn orientation(frame: &Com) -> u8 {
    // SAFETY: IWICBitmapFrameDecode::GetMetadataQueryReader (8), then
    // IWICMetadataQueryReader::GetMetadataByName (5) into a PROPVARIANT (24 bytes on x64).
    unsafe {
        let mut r = std::ptr::null_mut();
        let get: unsafe extern "system" fn(*mut c_void, Out) -> i32 = frame.slot(8);
        if get(frame.raw(), &mut r) < 0 || r.is_null() {
            return 1;
        }
        let reader = Com(r);
        let name: Vec<u16> = "System.Photo.Orientation\0".encode_utf16().collect();
        let mut var = [0u64; 3];
        let by_name: unsafe extern "system" fn(*mut c_void, *const u16, *mut c_void) -> i32 = reader.slot(5);
        let mut o = 1;
        if by_name(reader.raw(), name.as_ptr(), var.as_mut_ptr().cast()) >= 0 {
            let vt = var[0] as u16;
            if vt == VT_UI2 {
                o = (var[1] as u16) as u8;
            }
            PropVariantClear(var.as_mut_ptr().cast());
        }
        if (1..=8).contains(&o) { o } else { 1 }
    }
}

/// The codec's own downscale (IWICBitmapSourceTransform), if it has one and it helps. `None`
/// means: decode in full.
unsafe fn scaled(frame: &Com, fw: u32, fh: u32, fit: f64, orient: u8) -> Result<Option<Img>, String> {
    // Largest power-of-two shrink that still covers the screen at fit.
    let Some(s) = [8u32, 4, 2].into_iter().find(|&s| fit * s as f64 <= 1.0) else { return Ok(None) };
    // SAFETY: QueryInterface (slot 0) for the transform, then its slots: CopyPixels (3),
    // GetClosestSize (4), GetClosestPixelFormat (5).
    unsafe {
        let mut t = std::ptr::null_mut();
        let qi: unsafe extern "system" fn(*mut c_void, *const GUID, Out) -> i32 = frame.slot(0);
        if qi(frame.raw(), &IID_SOURCE_TRANSFORM, &mut t) < 0 || t.is_null() {
            return Ok(None);
        }
        let t = Com(t);
        let (mut w, mut h) = (fw.div_ceil(s), fh.div_ceil(s));
        let closest: unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32) -> i32 = t.slot(4);
        if closest(t.raw(), &mut w, &mut h) < 0 || w == 0 || h == 0 || w >= fw {
            return Ok(None);
        }
        let mut fmt = FMT_BGRA;
        let closest_fmt: unsafe extern "system" fn(*mut c_void, *mut GUID) -> i32 = t.slot(5);
        if closest_fmt(t.raw(), &mut fmt) < 0 {
            return Ok(None);
        }
        let bpp = match fmt {
            f if eq(&f, &FMT_BGRA) || eq(&f, &FMT_BGR32) || eq(&f, &FMT_PBGRA) => 4,
            f if eq(&f, &FMT_BGR24) => 3,
            f if eq(&f, &FMT_GRAY8) => 1,
            _ => return Ok(None),
        };
        let stride = (w as usize * bpp + 3) & !3;
        let len = stride * h as usize;
        let mut raw: Vec<u8> = Vec::with_capacity(len);
        let copy: unsafe extern "system" fn(*mut c_void, *const [i32; 4], u32, u32, *const GUID, u32, u32, u32, *mut u8) -> i32 =
            t.slot(3);
        let hr = copy(t.raw(), std::ptr::null(), w, h, &fmt, 0, stride as u32, len as u32, raw.as_mut_ptr());
        if hr < 0 {
            return Ok(None);
        }
        raw.set_len(len);
        let px = to_pbgra(&raw, w as usize, h as usize, stride, bpp, eq(&fmt, &FMT_BGRA));
        Ok(Some(Img { w, h, px, full_w: fw, full_h: fh, orient, full: false, thumb: false, id: next_id() }))
    }
}

fn to_pbgra(raw: &[u8], w: usize, h: usize, stride: usize, bpp: usize, straight_alpha: bool) -> Vec<u8> {
    if bpp == 4 && stride == w * 4 && !straight_alpha {
        let mut v = raw.to_vec();
        v.chunks_exact_mut(4).for_each(|p| p[3] = 255); // BGR32 leaves alpha undefined
        return v;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for row in raw.chunks_exact(stride).take(h) {
        match bpp {
            3 => row[..w * 3].chunks_exact(3).for_each(|p| out.extend_from_slice(&[p[0], p[1], p[2], 255])),
            1 => row[..w].iter().for_each(|&g| out.extend_from_slice(&[g, g, g, 255])),
            _ => row[..w * 4].chunks_exact(4).for_each(|p| {
                let a = p[3] as u32;
                let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                out.extend_from_slice(&[m(p[0]), m(p[1]), m(p[2]), p[3]]);
            }),
        }
    }
    out
}

fn eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
