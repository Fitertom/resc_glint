//! Installing into Windows: autostart, "Open with", and the default viewer for images.
//!
//! **Per user, no admin rights**: everything goes under HKCU.
//!
//! **The default is set through `UserChoice`, with its hash.** Since Windows 8 the choice for
//! an extension lives in `FileExts\.jpg\UserChoice` as a ProgId and a hash of (extension, user
//! SID, ProgId, the minute it was written, a fixed string from shell32). A key written without
//! the right hash is thrown away and the default reset. The hash is the one `SetUserFTA` /
//! `PS-SFTA` compute; it is written only when the user ticks the box, and checked afterwards
//! through the shell's own lookup. If Windows did not take it (a newer scheme, a protection
//! driver), Settings opens on our page, where the choice is one click.

use crate::files::EXTENSIONS;
use crate::win::wide;
use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{ASSOCSTR_PROGID, AssocQueryStringW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const PROGID: &str = "Glint.Image";
const CAPS: &str = r"Software\Glint\Capabilities";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const FILE_EXTS: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts";
/// The fixed part of every UserChoice hash, as shell32 carries it.
const EXPERIENCE: &str = "User Choice set via Windows User Experience {D18B6DD5-6124-4341-9318-804003BAFA0B}";

pub fn set_dword(key: &str, name: &str, value: u32) -> Result<(), String> {
    let (k, n) = (wide(key), wide(name));
    // SAFETY: NUL-terminated strings and a 4-byte value living through the call.
    let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), windows_sys::Win32::System::Registry::REG_DWORD, (&value as *const u32).cast(), 4) };
    if r == 0 { Ok(()) } else { Err(format!("registry {key}: error {r}")) }
}

pub fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let (k, v) = (wide(key), wide(value));
    let n = name.map(wide);
    // SAFETY: all strings are NUL-terminated and live through the call.
    let r = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()), REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32)
    };
    if r == 0 { Ok(()) } else { Err(format!("registry {key}: error {r}")) }
}

fn get(key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer is `len` bytes.
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) };
    (r == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

fn delete_value(key: &str, name: &str) {
    // SAFETY: NUL-terminated strings living through the call; a missing value is fine.
    unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(name).as_ptr()) };
}

fn notify() {
    // SAFETY: a plain shell notification.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

/// Starting with Windows, from any copy.
pub fn autostart() -> bool {
    get(RUN, "Glint").is_some()
}

/// Start `exe` resident at login.
pub fn set_autostart(on: bool, exe: &std::path::Path) -> Result<(), String> {
    if !on {
        delete_value(RUN, "Glint");
        return Ok(());
    }
    set(RUN, Some("Glint"), &format!("\"{}\" --background", exe.display()))
}

/// What the shell opens a `.jpg` with, right now.
pub fn is_default() -> bool {
    let ext = wide(".jpg");
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer is `len` characters.
    let hr = unsafe { AssocQueryStringW(0, ASSOCSTR_PROGID, ext.as_ptr(), std::ptr::null(), buf.as_mut_ptr(), &mut len) };
    hr >= 0 && String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]) == PROGID
}

/// Register the ProgId and "Open with" entries; with `on`, also make it the default for every
/// image type. Returns whether Windows took the default (checked, not assumed).
pub fn set_default(on: bool, exe: &std::path::Path) -> Result<bool, String> {
    if !on {
        // SAFETY: NUL-terminated strings living through each call; missing keys are fine.
        unsafe {
            for e in EXTENSIONS {
                let key = format!(r"{FILE_EXTS}\.{e}\UserChoice");
                if get(&key, "ProgId").as_deref() == Some(PROGID) {
                    RegDeleteKeyW(HKEY_CURRENT_USER, wide(&key).as_ptr());
                }
            }
            for k in [
                format!(r"Software\Classes\{PROGID}"),
                r"Software\Classes\Applications\glint.exe".into(),
                r"Software\Glint".into(),
                format!(r"Software\Classes\CLSID\{}", crate::com_server::CLSID_STR),
            ] {
                RegDeleteTreeW(HKEY_CURRENT_USER, wide(&k).as_ptr());
            }
        }
        for e in EXTENSIONS {
            delete_value(&format!(r"Software\Classes\.{e}\OpenWithProgids"), PROGID);
        }
        delete_value(r"Software\RegisteredApplications", "Glint");
        notify();
        return Ok(false);
    }
    let exe = exe.display().to_string();
    let open = format!("\"{exe}\" \"%1\"");
    let classes = format!(r"Software\Classes\{PROGID}");
    set(&classes, None, "Image")?;
    set(&format!(r"{classes}\DefaultIcon"), None, &format!("\"{exe}\",0"))?;
    set(&format!(r"{classes}\shell\open\command"), None, &open)?;
    // Explorer's "open" goes to the running viewer through COM, not through a new process; the
    // command line above stays for everything that runs the verb by hand.
    let clsid = format!(r"Software\Classes\CLSID\{}", crate::com_server::CLSID_STR);
    set(&clsid, None, "Glint open")?;
    set(&format!(r"{clsid}\LocalServer32"), None, &format!("\"{exe}\""))?;
    set(&format!(r"{classes}\shell\open\command"), Some("DelegateExecute"), crate::com_server::CLSID_STR)?;
    set(r"Software\Classes\Applications\glint.exe\shell\open\command", None, &open)?;
    set(CAPS, Some("ApplicationName"), "Glint")?;
    set(CAPS, Some("ApplicationDescription"), "Fast image viewer")?;
    for e in EXTENSIONS {
        set(&format!(r"Software\Classes\.{e}\OpenWithProgids"), Some(PROGID), "")?;
        set(&format!(r"{CAPS}\FileAssociations"), Some(&format!(".{e}")), PROGID)?;
    }
    set(r"Software\RegisteredApplications", Some("Glint"), CAPS)?;
    if let Some(sid) = user_sid() {
        // The hash holds the minute of writing: written across a minute's turn it would be
        // wrong, so a failed check is tried once more.
        for _ in 0..2 {
            for e in EXTENSIONS {
                let _ = write_user_choice(&format!(".{e}"), &sid);
            }
            notify();
            if is_default() {
                return Ok(true);
            }
        }
    }
    // Windows would not take it: its own page, on our entry.
    let page = wide("ms-settings:defaultapps?registeredAppUser=Glint");
    // SAFETY: NUL-terminated strings living through the call.
    unsafe { ShellExecuteW(std::ptr::null_mut(), std::ptr::null(), page.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    Ok(false)
}

fn write_user_choice(ext: &str, sid: &str) -> Result<(), String> {
    let key = format!(r"{FILE_EXTS}\{ext}\UserChoice");
    // THE OLD KEY IS DELETED, not overwritten: Windows puts a deny-set-value rule on it.
    // SAFETY: a NUL-terminated string living through the call.
    unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&key).as_ptr()) };
    let hash = user_choice_hash(&format!("{ext}{sid}{PROGID}{}{EXPERIENCE}", minute_now()).to_lowercase());
    set(&key, Some("ProgId"), PROGID)?;
    set(&key, Some("Hash"), &hash)
}

/// The user's SID, lower case, as the hash takes it.
fn user_sid() -> Option<String> {
    // SAFETY: the token is ours and closed below; the buffer is sized by the first call.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
        windows_sys::Win32::Foundation::CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return None;
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(s, n)).to_lowercase();
        LocalFree(s.cast());
        Some(sid)
    }
}

/// The current time as a FILETIME cut to the minute, in hex: the moment the hash names.
fn minute_now() -> String {
    let mut ft = windows_sys::Win32::Foundation::FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: writes the struct.
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime(&mut ft) };
    let t = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
    let t = t - t % 600_000_000;
    format!("{:08x}{:08x}", t >> 32, t & 0xffff_ffff)
}

/// The UserChoice hash: two rounds of a multiply-shift mix over the UTF-16 bytes, keyed by the
/// MD5 of the same bytes. All arithmetic is 32-bit and wraps.
fn user_choice_hash(base: &str) -> String {
    let mut data: Vec<u8> = base.encode_utf16().flat_map(u16::to_le_bytes).collect();
    data.extend_from_slice(&[0, 0]);
    let md5 = md5(&data);
    let len_base = data.len() as u32;
    let length = (if len_base & 4 <= 1 { 1 } else { 0 }) + (len_base >> 2) as i64 - 1;
    if length <= 1 {
        return String::new();
    }
    let word = |i: usize| -> u32 { data.get(i..i + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) };
    let m0 = u32::from_le_bytes([md5[0], md5[1], md5[2], md5[3]]) | 1;
    let m1 = u32::from_le_bytes([md5[4], md5[5], md5[6], md5[7]]) | 1;
    let rounds = (((length - 2) >> 1) + 1) as usize;
    let hi = |v: u32| v >> 16;

    let (md51, md52) = (m0.wrapping_add(0x69FB_0000), m1.wrapping_add(0x13DB_0000));
    let (mut out1, mut cache, mut out2) = (0u32, 0u32, 0u32);
    let mut p = 0;
    for _ in 0..rounds {
        let r0 = word(p).wrapping_add(out1);
        let r1 = word(p + 4);
        p += 8;
        let r2a = r0.wrapping_mul(md51).wrapping_sub(0x10FA_9605u32.wrapping_mul(hi(r0)));
        let r2b = 0x79F8_A395u32.wrapping_mul(r2a).wrapping_add(0x689B_6B9Fu32.wrapping_mul(hi(r2a)));
        let r3 = 0xEA97_0001u32.wrapping_mul(r2b).wrapping_sub(0x3C10_1569u32.wrapping_mul(hi(r2b)));
        let r4 = r3.wrapping_add(r1);
        let r5 = cache.wrapping_add(r3);
        let r6a = r4.wrapping_mul(md52).wrapping_sub(0x3CE8_EC25u32.wrapping_mul(hi(r4)));
        let r6b = 0x59C3_AF2Du32.wrapping_mul(r6a).wrapping_sub(0x2232_E0F1u32.wrapping_mul(hi(r6a)));
        out1 = 0x1EC9_0001u32.wrapping_mul(r6b).wrapping_add(0x35BD_1EC9u32.wrapping_mul(hi(r6b)));
        out2 = r5.wrapping_add(out1);
        cache = out2;
    }
    let (a0, a1) = (out1, out2);

    let (md51, md52) = (m0, m1);
    let (mut out1, mut cache, mut out2) = (0u32, 0u32, 0u32);
    let mut p = 0;
    for _ in 0..rounds {
        let r0 = word(p).wrapping_add(out1);
        p += 8;
        let r1a = r0.wrapping_mul(md51);
        let r1b = 0xB111_0000u32.wrapping_mul(r1a).wrapping_sub(0x3067_4EEFu32.wrapping_mul(hi(r1a)));
        let r2a = 0x5B9F_0000u32.wrapping_mul(r1b).wrapping_sub(0x78F7_A461u32.wrapping_mul(hi(r1b)));
        let r2b = 0x12CE_B96Du32.wrapping_mul(hi(r2a)).wrapping_sub(0x4693_0000u32.wrapping_mul(r2a));
        let r3 = 0x1D83_0000u32.wrapping_mul(r2b).wrapping_add(0x257E_1D83u32.wrapping_mul(hi(r2b)));
        let r4a = md52.wrapping_mul(r3.wrapping_add(word(p - 4)));
        let r4b = 0x16F5_0000u32.wrapping_mul(r4a).wrapping_sub(0x5D8B_E90Bu32.wrapping_mul(hi(r4a)));
        let r5a = 0x96FF_0000u32.wrapping_mul(r4b).wrapping_sub(0x2C7C_6901u32.wrapping_mul(hi(r4b)));
        let r5b = 0x2B89_0000u32.wrapping_mul(r5a).wrapping_add(0x7C93_2B89u32.wrapping_mul(hi(r5a)));
        out1 = 0x9F69_0000u32.wrapping_mul(r5b).wrapping_sub(0x405B_6097u32.wrapping_mul(hi(r5b)));
        out2 = out1.wrapping_add(cache).wrapping_add(r3);
        cache = out2;
    }
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(out1 ^ a0).to_le_bytes());
    bytes[4..].copy_from_slice(&(out2 ^ a1).to_le_bytes());
    base64(&bytes)
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// MD5, for the hash's key — here rather than through bcrypt.dll, which every launch (the
/// hand-over one too) would otherwise load at start.
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4,
        11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((input.len() as u64) * 8).to_le_bytes());
    let mut h = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        h = [h[0].wrapping_add(a), h[1].wrapping_add(b), h[2].wrapping_add(c), h[3].wrapping_add(d)];
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_reference() {
        let hex = |b: [u8; 16]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(md5(b"The quick brown fox jumps over the lazy dog")), "9e107d9d372bb6826bd81d3542a419d6");
    }

    /// The hash against the ones Windows itself wrote on this machine: each existing
    /// UserChoice's ProgId, with the minute its key was last written. Reads only.
    #[test]
    #[ignore = "reads this machine's registry"]
    fn hash_matches_what_windows_wrote() {
        use windows_sys::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryInfoKeyW};
        let sid = user_sid().unwrap();
        let mut checked = 0;
        for ext in [".jpg", ".png", ".txt", ".pdf", ".mp4", ".html", ".zip", ".mp3", ".gif", ".webp"] {
            let key = format!(r"{FILE_EXTS}\{ext}\UserChoice");
            let (Some(progid), Some(hash)) = (get(&key, "ProgId"), get(&key, "Hash")) else { continue };
            let mut hk: HKEY = std::ptr::null_mut();
            let mut ft = windows_sys::Win32::Foundation::FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            // SAFETY: a read-only key of ours, closed below.
            unsafe {
                assert_eq!(RegOpenKeyExW(HKEY_CURRENT_USER, wide(&key).as_ptr(), 0, KEY_READ, &mut hk), 0);
                let n = std::ptr::null_mut::<u32>();
                RegQueryInfoKeyW(hk, std::ptr::null_mut(), n, std::ptr::null(), n, n, n, n, n, n, n, &mut ft);
                RegCloseKey(hk);
            }
            let t = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
            let t = t - t % 600_000_000;
            let minute = format!("{:08x}{:08x}", t >> 32, t & 0xffff_ffff);
            let ours = user_choice_hash(&format!("{ext}{sid}{progid}{minute}{EXPERIENCE}").to_lowercase());
            println!("{ext:6} {progid:40} windows={hash} ours={ours}");
            assert_eq!(ours, hash, "{ext}");
            checked += 1;
        }
        assert!(checked > 0, "no UserChoice keys to check against");
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(&[0u8; 8]), "AAAAAAAAAAA=");
    }
}
