//! What is a ThumbnailCacheId made of, and where does its entry sit in thumbcache_*.db.
//! `cargo run --release --example thumb_probe -- <files...>`

use std::ffi::c_void;
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::core::GUID;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn slot<F: Copy>(p: *mut c_void, i: usize) -> F {
    unsafe { *(*(p as *const *const F)).add(i) }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileId {
    volume: u64,
    id: [u8; 16],
}

fn main() {
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    const CLSID_CACHE: GUID = GUID::from_u128(0x50ef4544_ac9f_4a8e_b21b_8a26180db13f);
    const IID_CACHE: GUID = GUID::from_u128(0xf676c15d_596a_4ce2_8234_33996f445db1);
    const IID_ITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
    let mut cache = std::ptr::null_mut();
    let t = Instant::now();
    let hr = unsafe { CoCreateInstance(&CLSID_CACHE, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_CACHE, &mut cache) };
    println!("cache object hr={hr:x} {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let db = std::fs::read(format!("{}/Microsoft/Windows/Explorer/thumbcache_256.db",std::env::var("LOCALAPPDATA").unwrap())).unwrap_or_default();
    for f in std::env::args().skip(1) {
        let w = wide(&f);
        let t = Instant::now();
        let mut item = std::ptr::null_mut();
        unsafe { windows_sys::Win32::UI::Shell::SHCreateItemFromParsingName(w.as_ptr(), std::ptr::null_mut(), &IID_ITEM, &mut item) };
        let parse = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let mut bmp = std::ptr::null_mut::<c_void>();
        let mut flags = 0u32;
        let mut id = [0u8; 16];
        let get: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, *mut *mut c_void, *mut u32, *mut [u8; 16]) -> i32 = unsafe { slot(cache, 3) };
        let hr = unsafe { get(cache, item, 256, 0x1, &mut bmp, &mut flags, &mut id) };
        let query = t.elapsed().as_secs_f64() * 1e3;
        let key = u64::from_le_bytes(id[..8].try_into().unwrap());

        // The file's identity.
        let h = unsafe { CreateFileW(w.as_ptr(), 0x80, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut()) };
        let mut fid = FileId::default();
        let mut bi: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let mut si: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
        unsafe {
            GetFileInformationByHandleEx(h, FileIdInfo, (&mut fid as *mut FileId).cast(), 24);
            GetFileInformationByHandleEx(h, FileBasicInfo, (&mut bi as *mut FILE_BASIC_INFO).cast(), size_of::<FILE_BASIC_INFO>() as u32);
            GetFileInformationByHandleEx(h, FileStandardInfo, (&mut si as *mut FILE_STANDARD_INFO).cast(), size_of::<FILE_STANDARD_INFO>() as u32);
            CloseHandle(h);
        }
        // Where it sits in the 256 db.
        let mut found = String::from("not in 256.db");
        let mut o = 24usize;
        while o + 56 <= db.len() && &db[o..o + 4] == b"CMMM" {
            let size = u32::from_le_bytes(db[o + 4..o + 8].try_into().unwrap()) as usize;
            let hash = u64::from_le_bytes(db[o + 8..o + 16].try_into().unwrap());
            if hash == key {
                let ids = u32::from_le_bytes(db[o + 16..o + 20].try_into().unwrap()) as usize;
                let pad = u32::from_le_bytes(db[o + 20..o + 24].try_into().unwrap()) as usize;
                let data = u32::from_le_bytes(db[o + 24..o + 28].try_into().unwrap());
                let ww = u32::from_le_bytes(db[o + 28..o + 32].try_into().unwrap());
                let hh = u32::from_le_bytes(db[o + 32..o + 36].try_into().unwrap());
                let d = o + 56 + ids + pad;
                found = format!("256.db @{o:#x} {ww}x{hh} data {data} B magic {:02x?}", &db[d..d + 4]);
                break;
            }
            if size == 0 {
                break;
            }
            o += size;
        }
        // Try the published Win7 algorithm with the encodings it could be using.
        let guid = volume_guid(&w);
        let mut dos = [0u16; 2];
        unsafe { windows_sys::Win32::System::WindowsProgramming::FileTimeToDosDateTime((&bi.LastWriteTime as *const i64).cast(), &mut dos[0], &mut dos[1]) };
        let ext: Vec<u8> = std::path::Path::new(&f).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default().encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let fid64 = &fid.id[..8];
        let mut rec = [0u8; 8];
        rec[..6].copy_from_slice(&fid.id[..6]);
        for (name, time) in [("date|time", (dos[0] as u32) << 16 | dos[1] as u32), ("time|date", (dos[1] as u32) << 16 | dos[0] as u32)] {
            for (fname, fidb) in [("fid64", fid64), ("fid128", &fid.id[..]), ("record", &rec[..])] {
                let mut h = 0x95E729BA2C37FD21u64;
                for part in [&guid[..], fidb, &ext[..], &time.to_le_bytes()[..]] {
                    h = mix(h, part);
                }
                println!("  {name} {fname}: {h:016x} {}", if h == key { "<== MATCH" } else { "" });
            }
        }
        // What the PIDL holds: DOS date/time at +8, BEEF0004 extension with the file id at
        // +0x14 and the modified-time remainder at +0x2a.
        let pidl = unsafe { ILCreateFromPathW(w.as_ptr()) };
        let mut last: &[u8] = &[];
        let mut p = pidl as *const u8;
        unsafe {
            loop {
                let cb = u16::from_le_bytes([*p, *p.add(1)]) as usize;
                if cb == 0 { break; }
                last = std::slice::from_raw_parts(p, cb);
                p = p.add(cb);
            }
        }
        let pos = last.windows(4).position(|x| x == 0xbeef0004u32.to_le_bytes()).map(|i| i - 4);
        let pidl_diff;
        if let Some(b) = pos {
            let e = &last[b..];
            pidl_diff = u32::from_le_bytes(e[0x2a..0x2e].try_into().unwrap());
            println!("  pidl: date {:04x} time {:04x} ext v{} fileref {:016x} diff {pidl_diff}", u16::from_le_bytes([last[8], last[9]]), u16::from_le_bytes([last[10], last[11]]), u16::from_le_bytes([e[2], e[3]]), u64::from_le_bytes(e[0x14..0x1c].try_into().unwrap()));
        }
        let mut dft = 0i64;
        unsafe { windows_sys::Win32::System::WindowsProgramming::DosDateTimeToFileTime(dos[0], dos[1], (&mut dft as *mut i64).cast()) };
        let diff = (dft - bi.LastWriteTime) as u32;
        println!("  my: date {:04x} time {:04x} diff {diff}", dos[0], dos[1]);
        let time = (dos[0] as u32) << 16 | dos[1] as u32;
        let mut h = 0x95E729BA2C37FD21u64;
        let mut t8 = time.to_le_bytes().to_vec();
        if diff != 0 { t8.extend_from_slice(&diff.to_le_bytes()); }
        for part in [&guid[..], fid64, &ext[..], &t8[..]] {
            h = mix(h, part);
        }
        println!("  win11: {h:016x} {}", if h == key { "<== MATCH" } else { "" });
        println!("{}", f.rsplit(['\\', '/']).next().unwrap());
        println!("  parse {parse:.2} ms  query {query:.2} ms hr={hr:x} flags={flags}  id={:02x?}", id);
        println!("  key {key:016x}  {found}");
        println!("  vol {:016x} fid {:02x?} mtime {:016x} size {} ", fid.volume, fid.id, bi.LastWriteTime, si.EndOfFile);
    }
}

fn mix(mut seed: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        seed ^= (seed >> 2).wrapping_add(seed.wrapping_mul(2080)).wrapping_add(b as u64);
    }
    seed
}

/// The volume's GUID in its in-memory layout, from the path's mount point.
fn volume_guid(path: &[u16]) -> [u8; 16] {
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
    let mut root = [0u16; 260];
    let mut name = [0u16; 64];
    unsafe {
        GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), 260);
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), 64);
    }
    // Volume{GUID} from the mount point name.
    let s = String::from_utf16_lossy(&name);
    let g: String = s.split('{').nth(1).unwrap_or("").chars().take(36).filter(|c| *c != '-').collect();
    let v = u128::from_str_radix(&g, 16).unwrap_or(0);
    let g = GUID::from_u128(v);
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..].copy_from_slice(&g.data4);
    println!("  volume {s}");
    out
}

windows_link::link!("shell32.dll" "system" fn ILCreateFromPathW(path: *const u16) -> *mut u8);
