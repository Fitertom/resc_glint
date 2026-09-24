//! Explorer's thumbnail cache read straight from its files, without the shell: parsing a
//! path into a shell item alone costs ~5 ms, this whole lookup a few dozen microseconds.
//!
//! The formats are undocumented; this follows what Windows 11 (database version 0x20) does,
//! reverse-engineered from `CFSFolder::_GetThumbnailCacheId` in windows.storage.dll. Anything
//! unexpected — another version, a file not on NTFS, a cloud placeholder, a miss — gives
//! `None`, and the caller asks the shell as before. A hit is checked against the entry's own
//! hash before it is trusted.
//!
//! - The key (ThumbnailCacheId) is a 64-bit hash, seeded, fed in turn with the volume GUID,
//!   the NTFS file reference, the extension as stored on disk (UTF-16) and the modified time:
//!   a DOS date/time (rounded up to 2 s) followed, when not exact, by the 100 ns it was
//!   rounded by.
//! - `thumbcache_idx.db` is a hash table: `N` entries of 72 bytes at the end of the file,
//!   entry `key % N` first and linear probing on, an empty slot ending the search. Each holds
//!   the key and one offset per database (16, 32, 48, 96, 256, 768, 1280, ... px).
//! - `thumbcache_<size>.db`: at that offset a `CMMM` entry, 56-byte header, identifier,
//!   padding, then the picture as JPEG, PNG or BMP.

use std::fs::File;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::WindowsProgramming::{DosDateTimeToFileTime, FileTimeToDosDateTime};

const SEED: u64 = 0x95E7_29BA_2C37_FD21;
const IDX_ENTRY: u64 = 72;
/// Databases worth a look, with their column in an index entry: 256 first (what Explorer
/// makes for large icons, and quick to decode), then the bigger ones.
const SIZES: [(&str, usize); 3] = [("256", 4), ("768", 5), ("1280", 6)];
/// Give up probing after this many occupied slots: the table is kept far from full.
const MAX_PROBE: u64 = 256;

/// The cached thumbnail's encoded bytes (JPEG, PNG or BMP), already turned upright.
pub fn lookup(path: &Path) -> Option<Vec<u8>> {
    let key = cache_id(path)?;
    let dir = dir()?;
    let idx = open(&dir.join("thumbcache_idx.db"))?;
    let len = idx.metadata().ok()?.len();
    let mut head = [0u8; 28];
    read_at(&idx, &mut head, 0)?;
    if &head[4..8] != b"IMMM" {
        return None;
    }
    let n = u32_at(&head, 24) as u64;
    if n == 0 || n * IDX_ENTRY > len {
        return None;
    }
    let base = len - n * IDX_ENTRY;
    let mut e = [0u8; IDX_ENTRY as usize];
    let mut slot = key % n;
    let mut probes = 0;
    loop {
        read_at(&idx, &mut e, base + slot * IDX_ENTRY)?;
        match u64_at(&e, 0) {
            k if k == key => break,
            0 => return None,
            _ => {}
        }
        probes += 1;
        if probes >= MAX_PROBE.min(n) {
            return None;
        }
        slot = (slot + 1) % n;
    }
    SIZES.iter().find_map(|&(size, col)| match u32_at(&e, 16 + col * 4) {
        0 | u32::MAX => None,
        at => entry(&dir.join(format!("thumbcache_{size}.db")), at as u64, key),
    })
}

/// The picture stored at `at` in database `db`, if the entry there is the one for `key`.
fn entry(db: &Path, at: u64, key: u64) -> Option<Vec<u8>> {
    let f = open(db)?;
    let mut h = [0u8; 56];
    read_at(&f, &mut h, at)?;
    if &h[..4] != b"CMMM" || u64_at(&h, 8) != key {
        return None;
    }
    let (name, pad, size) = (u32_at(&h, 16) as u64, u32_at(&h, 20) as u64, u32_at(&h, 24) as usize);
    if size == 0 || size > 32 << 20 {
        return None;
    }
    let mut data = vec![0u8; size];
    read_at(&f, &mut data, at + 56 + name + pad)?;
    Some(data)
}

/// The ThumbnailCacheId Explorer gives the file, as long as it is an ordinary one on NTFS.
pub fn cache_id(path: &Path) -> Option<u64> {
    let wpath = crate::files::wide(path.as_os_str());
    // SAFETY: plain calls with buffers that outlive them; the handle is closed here.
    let info = unsafe {
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        const READ_ATTRIBUTES: u32 = 0x80;
        let h = CreateFileW(wpath.as_ptr(), READ_ATTRIBUTES, share, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut());
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(h, &mut info);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        info
    };
    let file_ref = (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64;
    if file_ref == 0 {
        return None;
    }
    let guid = volume_guid(&wpath, info.dwVolumeSerialNumber)?;
    let ext: Vec<u8> = match path.extension() {
        Some(e) => std::iter::once('.' as u16).chain(e.to_string_lossy().encode_utf16()).flat_map(u16::to_le_bytes).collect(),
        None => Vec::new(),
    };
    // The time as the shell's item keeps it: DOS date and time (rounded up to 2 s), then the
    // 100 ns the rounding added, when it added any.
    let (mut date, mut time) = (0u16, 0u16);
    let mut back = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: plain conversions between stack values.
    unsafe {
        if FileTimeToDosDateTime(&info.ftLastWriteTime, &mut date, &mut time) == 0 || DosDateTimeToFileTime(date, time, &mut back) == 0 {
            return None;
        }
    }
    let ft = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    let rounded = ft(back).wrapping_sub(ft(info.ftLastWriteTime)) as u32;
    let mut stamp = ((date as u32) << 16 | time as u32).to_le_bytes().to_vec();
    if rounded != 0 {
        stamp.extend_from_slice(&rounded.to_le_bytes());
    }
    let mut hash = SEED;
    for part in [&guid[..], &file_ref.to_le_bytes(), &ext, &stamp] {
        hash = mix(hash, part);
    }
    Some(hash)
}

fn mix(mut seed: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        seed ^= (seed >> 2).wrapping_add(seed.wrapping_mul(2080)).wrapping_add(b as u64);
    }
    seed
}

/// The GUID of the volume holding `path` (in its in-memory byte order), remembered per volume
/// serial number: finding it goes through the mount manager.
fn volume_guid(path: &[u16], serial: u32) -> Option<[u8; 16]> {
    static KNOWN: Mutex<Vec<(u32, [u8; 16])>> = Mutex::new(Vec::new());
    if let Some(&(_, g)) = KNOWN.lock().ok()?.iter().find(|(s, _)| *s == serial) {
        return Some(g);
    }
    let mut root = [0u16; 260];
    let mut name = [0u16; 64];
    // SAFETY: both buffers are as long as the calls are told.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0
            || GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32) == 0
        {
            return None;
        }
    }
    // \\?\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}\
    let name = String::from_utf16_lossy(&name);
    let hex: String = name.split('{').nth(1)?.chars().take(36).filter(|c| *c != '-').collect();
    let v = u128::from_str_radix(&hex, 16).ok()?;
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&((v >> 96) as u32).to_le_bytes());
    g[4..6].copy_from_slice(&((v >> 80) as u16).to_le_bytes());
    g[6..8].copy_from_slice(&((v >> 64) as u16).to_le_bytes());
    g[8..].copy_from_slice(&(v as u64).to_be_bytes());
    KNOWN.lock().ok()?.push((serial, g));
    Some(g)
}

fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join(r"Microsoft\Windows\Explorer"))).as_ref()
}

/// Opened per lookup and shared every way, so Explorer can go on writing, and clearing the
/// cache can delete the files under us.
fn open(path: &Path) -> Option<File> {
    std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).open(path).ok()
}

fn read_at(f: &File, buf: &mut [u8], mut at: u64) -> Option<()> {
    let mut done = 0;
    while done < buf.len() {
        match f.seek_read(&mut buf[done..], at) {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                done += n;
                at += n as u64;
            }
        }
    }
    Some(())
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
