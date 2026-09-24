//! The caption and the info pills, drawn with GDI into a bitmap that goes up as a texture.
//!
//! **GDI and not a font atlas**: the system's own text renderer is already in memory, draws
//! Cyrillic and every other script a file name can hold, and adds nothing to the exe. The
//! caption is opaque, so its text gets ClearType like any system caption; the pills float over
//! the photo and get grayscale antialiasing, turned into alpha here.
//!
//! Colours and proportions are the editor's (`resc_engine` `ui/chrome.rs`): the caption ground,
//! the button width of the system's own, the red close hover.

use std::ffi::c_void;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::*;

type Rgb = (u8, u8, u8);
pub const CHROME_BG: Rgb = (0x13, 0x13, 0x14);
pub const CHROME_BG_REF: u32 = CHROME_BG.0 as u32 | (CHROME_BG.1 as u32) << 8 | (CHROME_BG.2 as u32) << 16;
const HOVER: Rgb = (0x2a, 0x2a, 0x2d);
const PRESSED: Rgb = (0x33, 0x33, 0x37);
const CLOSE_HOT: Rgb = (0xc4, 0x2b, 0x1c);
const TEXT: Rgb = (0xc9, 0xc9, 0xcc);
const TEXT_DIM: Rgb = (0x86, 0x86, 0x8c);
const TEXT_HEAD: Rgb = (0xe6, 0xe6, 0xea);
const BG_PANEL: Rgb = (0x24, 0x24, 0x26);
const BG_ROW_ALT: Rgb = (0x2a, 0x2a, 0x2d);
const LINE: Rgb = (0x3a, 0x3a, 0x3e);
const ACCENT: Rgb = (0x3d, 0x8b, 0xd4);
const ACCENT_HOT: Rgb = (0x4a, 0x9a, 0xe4);
const BOX: Rgb = (0x5a, 0x5a, 0x60);
const BG_BAR: Rgb = (0x1f, 0x1f, 0x21);
const HOVER_BAR: Rgb = (0x3a, 0x3a, 0x3f);

/// Caption height and button width at 96 dpi: the system caption's, so the buttons sit where a
/// hand reaches for them.
const CAPTION_H: i32 = 32;
const BUTTON_W: i32 = 46;
const LOGO_PX: i32 = 20;
const TEXT_PX: i32 = 13;

/// The logo, premultiplied BGRA 64×64, rasterised from `assets/logo.svg` by `build.rs`.
const LOGO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.bgra"));
const LOGO_SIDE: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Settings,
    Min,
    Max,
    Close,
}

const BUTTONS: [Button; 4] = [Button::Settings, Button::Min, Button::Max, Button::Close];

/// What can be clicked on the setup card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardItem {
    Install,
    Autostart,
    Default,
    Apply,
    Skip,
}

/// The buttons of the bar at the bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Prev,
    Next,
    ZoomOut,
    ZoomIn,
    Fit,
    Actual,
    Rotate,
    Fullscreen,
    Delete,
}

/// A floating panel as drawn: premultiplied pixels, and where its parts are in its own pixels.
pub struct Panel<T> {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
    pub items: Vec<(T, RECT)>,
}

pub struct Painter {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u8,
    bw: i32,
    bh: i32,
    dpi: u32,
    text: HFONT,
    icons: HFONT,
    /// The toolbar's: its glyphs and its labels. Made with the others, per DPI: made on every
    /// toolbar drawn, they were most of its cost.
    bar_glyphs: HFONT,
    bar_text: HFONT,
    logo: Vec<u8>,
    logo_px: i32,
}

pub fn scaled(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

pub fn caption_h(dpi: u32) -> i32 {
    scaled(CAPTION_H, dpi)
}

pub fn buttons_w(dpi: u32) -> i32 {
    scaled(BUTTON_W, dpi) * BUTTONS.len() as i32
}

/// Which caption button is under a client point, if any.
pub fn button_at(x: i32, y: i32, width: i32, dpi: u32) -> Option<Button> {
    let bw = scaled(BUTTON_W, dpi);
    let n = BUTTONS.len() as i32;
    if y < 0 || y >= caption_h(dpi) || x < width - bw * n || x >= width {
        return None;
    }
    Some(BUTTONS[((x - (width - bw * n)) / bw).clamp(0, n - 1) as usize])
}

impl Painter {
    pub fn new() -> Painter {
        // SAFETY: a memory DC of our own.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let mut p = Painter {
            dc,
            bmp: std::ptr::null_mut(),
            bits: std::ptr::null_mut(),
            bw: 0,
            bh: 0,
            dpi: 0,
            text: std::ptr::null_mut(),
            icons: std::ptr::null_mut(),
            bar_glyphs: std::ptr::null_mut(),
            bar_text: std::ptr::null_mut(),
            logo: Vec::new(),
            logo_px: 0,
        };
        p.set_dpi(96);
        p
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        // SAFETY: fonts of ours, deleted once.
        unsafe {
            for f in [self.text, self.icons, self.bar_glyphs, self.bar_text] {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            self.text = font(scaled(TEXT_PX, dpi), "Segoe UI", CLEARTYPE_QUALITY as u32);
            // The caption glyphs of Windows 11, or of Windows 10 where that font is missing.
            self.icons = font(scaled(10, dpi), "Segoe Fluent Icons", CLEARTYPE_QUALITY as u32);
            SelectObject(self.dc, self.icons);
            let mut face = [0u16; 64];
            let n = GetTextFaceW(self.dc, face.len() as i32, face.as_mut_ptr());
            if String::from_utf16_lossy(&face[..n.max(1) as usize - 1]) != "Segoe Fluent Icons" {
                DeleteObject(self.icons);
                self.icons = font(scaled(10, dpi), "Segoe MDL2 Assets", CLEARTYPE_QUALITY as u32);
            }
            self.bar_glyphs = font(scaled(16, dpi), self.icon_face(), CLEARTYPE_QUALITY as u32);
            self.bar_text = font(scaled(13, dpi), "Segoe UI", CLEARTYPE_QUALITY as u32);
        }
        self.logo_px = scaled(LOGO_PX, dpi).min(LOGO_SIDE as i32);
        self.logo = shrink(LOGO, LOGO_SIDE, self.logo_px as usize);
    }

    /// The bitmap at least `w × h`.
    fn canvas(&mut self, w: i32, h: i32) {
        if w <= self.bw && h <= self.bh {
            return;
        }
        let (w, h) = (w.max(self.bw), h.max(self.bh));
        // SAFETY: the header is filled here; the old bitmap is deleted after it is deselected.
        unsafe {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            SelectObject(self.dc, bmp);
            if !self.bmp.is_null() {
                DeleteObject(self.bmp);
            }
            self.bmp = bmp;
            self.bits = bits as *mut u8;
        }
        self.bw = w;
        self.bh = h;
    }

    fn fill(&self, r: RECT, c: Rgb) {
        // SAFETY: a brush made and deleted around one call on our DC.
        unsafe {
            let b = CreateSolidBrush(rgb(c));
            FillRect(self.dc, &r, b);
            DeleteObject(b);
        }
    }

    /// Draw `s` into `r` with DrawText flags; returns the width it took.
    fn text(&self, s: &str, f: HFONT, c: Rgb, mut r: RECT, flags: u32) -> i32 {
        // NOTHING FOR AN EMPTY STRING: an empty Vec's pointer dangles, and DrawTextW with an
        // ellipsis flag reads through it even at length zero (crashed the hidden viewer).
        if s.is_empty() {
            return 0;
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        // SAFETY: our DC and font; the string lives through the calls.
        unsafe {
            SelectObject(self.dc, f);
            SetTextColor(self.dc, rgb(c));
            SetBkMode(self.dc, TRANSPARENT as i32);
            DrawTextW(self.dc, w.as_ptr(), w.len() as i32, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
        }
        r.right - r.left
    }

    fn measure(&self, s: &str, f: HFONT) -> (i32, i32) {
        if s.is_empty() {
            return (0, 0);
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: as in `text`.
        unsafe {
            SelectObject(self.dc, f);
            DrawTextW(self.dc, w.as_ptr(), w.len() as i32, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
        }
        (r.right - r.left, r.bottom - r.top)
    }

    /// The bitmap's top-left `w × h` as tight rows.
    fn take(&self, w: i32, h: i32) -> Vec<u8> {
        // SAFETY: GDI is flushed so the bits are final; the DIB is `bw × bh`, larger or equal.
        unsafe {
            GdiFlush();
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                let row = std::slice::from_raw_parts(self.bits.add((y * self.bw * 4) as usize), (w * 4) as usize);
                out.extend_from_slice(row);
            }
            out
        }
    }

    /// The caption: logo, file name, info at the right, then minimise / maximise / close.
    pub fn caption(&mut self, dpi: u32, width: i32, title: &str, info: &str, hot: Option<Button>, pressed: bool, maximized: bool) -> Vec<u8> {
        self.set_dpi(dpi);
        let h = caption_h(dpi);
        self.canvas(width, h);
        let rect = |l, r| RECT { left: l, top: 0, right: r, bottom: h };
        self.fill(rect(0, width), CHROME_BG);

        let bw = scaled(BUTTON_W, dpi);
        let x0 = width - bw * BUTTONS.len() as i32;
        let glyphs = ['\u{E713}', '\u{E921}', if maximized { '\u{E923}' } else { '\u{E922}' }, '\u{E8BB}'];
        for (k, (b, g)) in BUTTONS.into_iter().zip(glyphs).enumerate() {
            let r = rect(x0 + bw * k as i32, x0 + bw * (k as i32 + 1));
            let is_hot = hot == Some(b);
            if is_hot {
                self.fill(r, if b == Button::Close { CLOSE_HOT } else if pressed { PRESSED } else { HOVER });
            }
            let c = if is_hot { if b == Button::Close { (255, 255, 255) } else { TEXT_HEAD } } else { TEXT_DIM };
            self.text(&g.to_string(), self.icons, c, r, DT_CENTER);
        }

        let pad = scaled(10, dpi);
        let title_x = pad + self.logo_px + scaled(8, dpi);
        let (iw, _) = self.measure(info, self.text);
        let info_r = x0 - scaled(14, dpi);
        let info_l = (info_r - iw).max(title_x + scaled(60, dpi));
        if info_r > info_l {
            self.text(info, self.text, TEXT_DIM, rect(info_l, info_r), DT_RIGHT | DT_END_ELLIPSIS);
        }
        let title_r = info_l - scaled(16, dpi);
        if title_r > title_x {
            self.text(title, self.text, TEXT, rect(title_x, title_r), DT_LEFT | DT_END_ELLIPSIS);
        }

        let mut px = self.take(width, h);
        let ly = (h - self.logo_px) / 2;
        blend(&mut px, width as usize, &self.logo, self.logo_px as usize, pad as usize, ly as usize);
        px.chunks_exact_mut(4).for_each(|p| p[3] = 255); // GDI leaves alpha at zero
        px
    }

    /// The setup card: what makes the viewer part of Windows, one row each, and one button.
    /// Shown once on the first start, and from the gear in the caption.
    pub fn card(&mut self, dpi: u32, rows: &[(CardItem, bool)], hot: Option<CardItem>, note: &str) -> Panel<CardItem> {
        self.set_dpi(dpi);
        let s = |v| scaled(v, dpi);
        let (w, h) = (s(460), s(166) + rows.len() as i32 * s(48));
        self.canvas(w, h);
        // SAFETY: fonts made and deleted here, on our DC.
        let (title, body, check) = unsafe {
            (
                font(s(20), "Segoe UI Semibold", CLEARTYPE_QUALITY as u32),
                font(s(13), "Segoe UI", CLEARTYPE_QUALITY as u32),
                font(s(12), self.icon_face(), CLEARTYPE_QUALITY as u32),
            )
        };
        let r = |l, t, rr, b| RECT { left: l, top: t, right: rr, bottom: b };
        self.fill(r(0, 0, w, h), BG_PANEL);
        let pad = s(24);
        let logo = s(40).min(LOGO_SIDE as i32);
        self.text("Glint", title, TEXT_HEAD, r(pad + logo + s(14), s(22), w - pad, s(50)), DT_LEFT);
        self.text("Fast image viewer", body, TEXT_DIM, r(pad + logo + s(14), s(48), w - pad, s(68)), DT_LEFT);

        let mut items = Vec::new();
        for (k, &(item, on)) in rows.iter().enumerate() {
            let (label, sub) = match item {
                CardItem::Install => ("Install on this PC", "Programs folder, Start menu, uninstall from Settings"),
                CardItem::Autostart => ("Start with Windows", "photos open instantly · ~2 MB in memory"),
                _ => ("Open images with Glint", "jpg, png, gif, webp, heic, tiff…"),
            };
            let top = s(90) + k as i32 * s(48);
            let row = r(s(12), top, w - s(12), top + s(44));
            if hot == Some(item) {
                self.round(row, s(6), BG_ROW_ALT);
            }
            let bs = s(18);
            let bx = r(pad, top + (s(44) - bs) / 2, pad + bs, top + (s(44) + bs) / 2);
            if on {
                self.round(bx, s(4), ACCENT);
                self.text("\u{E73E}", check, (255, 255, 255), bx, DT_CENTER);
            } else {
                self.round(bx, s(4), BOX);
                self.round(r(bx.left + s(1), bx.top + s(1), bx.right - s(1), bx.bottom - s(1)), s(3), if hot == Some(item) { BG_ROW_ALT } else { BG_PANEL });
            }
            let lx = pad + bs + s(12);
            self.text(label, body, TEXT, r(lx, top + s(4), w - pad, top + s(24)), DT_LEFT);
            self.text(sub, body, TEXT_DIM, r(lx, top + s(22), w - pad, top + s(42)), DT_LEFT | DT_END_ELLIPSIS);
            items.push((item, row));
        }

        let by = h - pad - s(34);
        let apply = r(w - pad - s(120), by, w - pad, by + s(34));
        self.round(apply, s(6), if hot == Some(CardItem::Apply) { ACCENT_HOT } else { ACCENT });
        self.text("Apply", body, (255, 255, 255), apply, DT_CENTER);
        items.push((CardItem::Apply, apply));
        let skip = r(apply.left - s(12) - s(90), by, apply.left - s(12), by + s(34));
        if hot == Some(CardItem::Skip) {
            self.round(skip, s(6), BG_ROW_ALT);
        }
        self.text("Not now", body, if hot == Some(CardItem::Skip) { TEXT_HEAD } else { TEXT_DIM }, skip, DT_CENTER);
        items.push((CardItem::Skip, skip));
        if !note.is_empty() {
            self.text(note, body, TEXT_DIM, r(pad, by, skip.left - s(8), by + s(34)), DT_LEFT | DT_END_ELLIPSIS);
        }
        // SAFETY: made above.
        unsafe {
            for f in [title, body, check] {
                DeleteObject(f);
            }
        }

        let mut px = self.take(w, h);
        let lg = shrink(LOGO, LOGO_SIDE, logo as usize);
        blend(&mut px, w as usize, &lg, logo as usize, pad as usize, s(24) as usize);
        panel_mask(&mut px, w, h, s(10) as f32, 1.0);
        Panel { w: w as u32, h: h as u32, px, items }
    }

    /// The bar at the bottom: flip, zoom, fit, 1:1, turn, fullscreen, delete.
    pub fn toolbar(&mut self, dpi: u32, hot: Option<Tool>, fullscreen: bool, zoom: &str) -> Panel<Tool> {
        self.set_dpi(dpi);
        let s = |v| scaled(v, dpi);
        let groups: &[&[Tool]] = &[
            &[Tool::Prev, Tool::Next],
            &[Tool::ZoomOut, Tool::ZoomIn],
            &[Tool::Fit, Tool::Actual],
            &[Tool::Rotate, Tool::Fullscreen],
            &[Tool::Delete],
        ];
        let (bw, bh, pad, gap) = (s(40), s(36), s(6), s(13));
        let zoom_w = s(52);
        let n: i32 = groups.iter().map(|g| g.len() as i32).sum();
        let w = pad * 2 + n * bw + (groups.len() as i32 - 1) * gap + zoom_w;
        let h = bh + pad * 2;
        self.canvas(w, h);
        let (glyphs, body) = (self.bar_glyphs, self.bar_text);
        let r = |l, t, rr, b| RECT { left: l, top: t, right: rr, bottom: b };
        self.fill(r(0, 0, w, h), BG_BAR);
        let mut items = Vec::new();
        let mut x = pad;
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                self.fill(r(x + gap / 2, pad + s(8), x + gap / 2 + 1, h - pad - s(8)), LINE);
                x += gap;
            }
            for &t in g.iter() {
                let b = r(x, pad, x + bw, pad + bh);
                let is_hot = hot == Some(t);
                if is_hot {
                    self.round(b, s(6), if t == Tool::Delete { CLOSE_HOT } else { HOVER_BAR });
                }
                let c = if is_hot { (255, 255, 255) } else { TEXT };
                match t {
                    Tool::Actual => {
                        self.text("1:1", body, c, b, DT_CENTER);
                    }
                    _ => {
                        let g = match t {
                            Tool::Prev => '\u{E76B}',
                            Tool::Next => '\u{E76C}',
                            Tool::ZoomOut => '\u{E71F}',
                            Tool::ZoomIn => '\u{E8A3}',
                            Tool::Fit => '\u{E9A6}',
                            Tool::Rotate => '\u{E7AD}',
                            Tool::Fullscreen if fullscreen => '\u{E73F}',
                            Tool::Fullscreen => '\u{E740}',
                            _ => '\u{E74D}',
                        };
                        self.text(&g.to_string(), glyphs, c, b, DT_CENTER);
                    }
                }
                items.push((t, b));
                x += bw;
                // The zoom level between its two buttons.
                if t == Tool::ZoomOut {
                    self.text(zoom, body, TEXT_DIM, r(x, pad, x + zoom_w, pad + bh), DT_CENTER);
                    x += zoom_w;
                }
            }
        }
        let mut px = self.take(w, h);
        panel_mask(&mut px, w, h, s(10) as f32, 0.94);
        Panel { w: w as u32, h: h as u32, px, items }
    }

    /// The icon font there is: Windows 11's, or Windows 10's.
    fn icon_face(&self) -> &'static str {
        let mut face = [0u16; 64];
        // SAFETY: our DC and font.
        let n = unsafe {
            SelectObject(self.dc, self.icons);
            GetTextFaceW(self.dc, face.len() as i32, face.as_mut_ptr())
        };
        if String::from_utf16_lossy(&face[..(n.max(1) - 1) as usize]) == "Segoe Fluent Icons" { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }
    }

    /// A filled rounded rectangle, antialiased, straight into the bitmap over what is there.
    /// GDI's own `RoundRect` has no antialiasing: its corners came out as steps.
    fn round(&self, r: RECT, radius: i32, c: Rgb) {
        // SAFETY: GDI is flushed before the bits are touched; every write is inside the DIB.
        unsafe {
            GdiFlush();
            let (fw, fh, rad) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32, radius as f32);
            for y in r.top.max(0)..r.bottom.min(self.bh) {
                for x in r.left.max(0)..r.right.min(self.bw) {
                    let cov = round_cover((x - r.left) as f32 + 0.5, (y - r.top) as f32 + 0.5, fw, fh, rad, 0.0);
                    if cov <= 0.0 {
                        continue;
                    }
                    let p = std::slice::from_raw_parts_mut(self.bits.add(((y * self.bw + x) * 4) as usize), 4);
                    for (i, v) in [c.2, c.1, c.0].into_iter().enumerate() {
                        p[i] = (p[i] as f32 + (v as f32 - p[i] as f32) * cov).round() as u8;
                    }
                }
            }
        }
    }

    /// Text on a rounded dark pill, premultiplied with alpha: floats over the photo.
    pub fn pill(&mut self, dpi: u32, s: &str) -> (u32, u32, Vec<u8>) {
        self.set_dpi(dpi);
        // Grayscale antialiasing: ClearType's coloured fringes assume an opaque ground.
        // SAFETY: a font of ours, deleted after use.
        let f = unsafe { font(scaled(TEXT_PX, dpi), "Segoe UI", ANTIALIASED_QUALITY as u32) };
        let (tw, th) = self.measure(s, f);
        let (px_, py) = (scaled(12, dpi), scaled(6, dpi));
        let (w, h) = ((tw + px_ * 2).clamp(1, 8192), th + py * 2);
        self.canvas(w, h);
        let r = RECT { left: 0, top: 0, right: w, bottom: h };
        self.fill(r, (0, 0, 0));
        self.text(s, f, (255, 255, 255), RECT { left: px_, top: 0, right: w - px_, bottom: h }, DT_LEFT);
        // SAFETY: made above.
        unsafe { DeleteObject(f) };
        let mut px = self.take(w, h);
        let radius = scaled(7, dpi) as f32;
        let (fw, fh) = (w as f32, h as f32);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &mut px[(y * w as usize + x) * 4..][..4];
                let cov = p[1] as f32 / 255.0;
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                let dx = (radius - cx).max(cx - (fw - radius)).max(0.0);
                let dy = (radius - cy).max(cy - (fh - radius)).max(0.0);
                let inside = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
                let a = (cov + 0.62 * inside * (1.0 - cov)).min(1.0);
                let t = [TEXT_HEAD.2, TEXT_HEAD.1, TEXT_HEAD.0];
                for c in 0..3 {
                    p[c] = (t[c] as f32 * cov).round() as u8;
                }
                p[3] = (a * 255.0).round() as u8;
            }
        }
        (w as u32, h as u32, px)
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        // SAFETY: objects of ours, deleted once.
        unsafe {
            for o in [self.text, self.icons, self.bar_glyphs, self.bar_text, self.bmp] {
                if !o.is_null() {
                    DeleteObject(o);
                }
            }
            DeleteDC(self.dc);
        }
    }
}

/// How much of the pixel at (cx, cy) a `w × h` rectangle rounded by `radius` covers, the
/// rectangle shrunk by `inset` on every side.
fn round_cover(cx: f32, cy: f32, w: f32, h: f32, radius: f32, inset: f32) -> f32 {
    let rr = (radius - inset).max(0.0);
    let dx = (inset + rr - cx).max(cx - (w - inset - rr)).max(0.0);
    let dy = (inset + rr - cy).max(cy - (h - inset - rr)).max(0.0);
    if cx < inset || cy < inset || cx > w - inset || cy > h - inset {
        return 0.0;
    }
    if dx == 0.0 && dy == 0.0 {
        return 1.0;
    }
    (rr - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0)
}

/// Turn an opaque panel into a floating one: rounded, antialiased corners with a one-pixel rim,
/// premultiplied at `opacity`.
fn panel_mask(px: &mut [u8], w: i32, h: i32, radius: f32, opacity: f32) {
    let (fw, fh) = (w as f32, h as f32);
    let line = [LINE.2, LINE.1, LINE.0];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (outer, inner) = (round_cover(cx, cy, fw, fh, radius, 0.0), round_cover(cx, cy, fw, fh, radius, 1.0));
            let p = &mut px[(y * w as usize + x) * 4..][..4];
            let rim = outer - inner;
            let a = outer * opacity;
            for c in 0..3 {
                p[c] = ((p[c] as f32 * inner + line[c] as f32 * rim) * opacity).round() as u8;
            }
            p[3] = (a * 255.0).round() as u8;
        }
    }
}

fn rgb(c: Rgb) -> u32 {
    c.0 as u32 | (c.1 as u32) << 8 | (c.2 as u32) << 16
}

unsafe fn font(px: i32, face: &str, quality: u32) -> HFONT {
    let f = crate::win::wide(face);
    // SAFETY: the face name lives through the call.
    unsafe { CreateFontW(-px, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, quality, 0, f.as_ptr()) }
}

/// Premultiplied BGRA `src × src` shrunk to `dst × dst` by area — each output pixel the mean of
/// what it covers. Premultiplied, so the transparent ground does not bleed into the rim.
fn shrink(px: &[u8], src: usize, dst: usize) -> Vec<u8> {
    let k = src as f32 / dst as f32;
    let mut out = vec![0u8; dst * dst * 4];
    for y in 0..dst {
        for x in 0..dst {
            let (x0, x1, y0, y1) = (x as f32 * k, (x + 1) as f32 * k, y as f32 * k, (y + 1) as f32 * k);
            let (mut acc, mut area) = ([0.0f32; 4], 0.0);
            for sy in y0 as usize..(y1.ceil() as usize).min(src) {
                let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                for sx in x0 as usize..(x1.ceil() as usize).min(src) {
                    let w = wy * (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                    let p = &px[(sy * src + sx) * 4..][..4];
                    for c in 0..4 {
                        acc[c] += p[c] as f32 * w;
                    }
                    area += w;
                }
            }
            for c in 0..4 {
                out[(y * dst + x) * 4 + c] = (acc[c] / area).round() as u8;
            }
        }
    }
    out
}

/// Premultiplied `src` (side × side) over `dst` (rows of `width`) at (x, y).
fn blend(dst: &mut [u8], width: usize, src: &[u8], side: usize, x: usize, y: usize) {
    for sy in 0..side {
        for sx in 0..side {
            let s = &src[(sy * side + sx) * 4..][..4];
            let o = ((y + sy) * width + x + sx) * 4;
            let Some(d) = dst.get_mut(o..o + 4) else { continue };
            let inv = 255 - s[3] as u32;
            for c in 0..3 {
                d[c] = (s[c] as u32 + (d[c] as u32 * inv + 127) / 255).min(255) as u8;
            }
        }
    }
}
