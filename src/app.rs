//! The viewer: which file is shown, how (zoom, pan, turn), what to decode next, and the frame.

use crate::config::Config;
use crate::files;
use crate::gpu::{Gpu, IMAGE_SLOTS, Quad, SLOT_BAR, SLOT_CAPTION, SLOT_CARD, SLOT_INFO};
use crate::loader::{FULL, Key, Loader, Msg, PREVIEW, THUMB, drop_later};
use crate::ui::{self, Button, CardItem, Painter, Tool};
use crate::wic::Img;
use crate::win::{self, Btn, Ev, Fullscreen};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// Zoom per wheel notch and per key press.
const STEP: f64 = 1.2;
const MAX_ZOOM: f64 = 64.0;

#[derive(Default)]
struct Slot {
    thumb: Option<Arc<Img>>,
    preview: Option<Arc<Img>>,
    full: Option<Arc<Img>>,
    err: Option<String>,
    /// No thumbnail to be had: not cached by the shell, none in the file. Not an error.
    thumb_tried: bool,
}

impl Slot {
    fn best(&self) -> Option<&Arc<Img>> {
        self.full.as_ref().or(self.preview.as_ref()).or(self.thumb.as_ref())
    }
    /// Real pixels, not a stand-in.
    fn decoded(&self) -> bool {
        self.full.is_some() || self.preview.is_some()
    }
}

/// What a GPU slot holds — facts about the pixels, not the pixels: holding those alive kept a
/// 96 MB full decode in memory after the cache had let it go, and freed it in the key handler.
#[derive(Clone)]
struct Tex {
    path: Arc<Path>,
    id: u64,
    /// Thumbnail 0, preview 1, full 2.
    rank: u8,
    /// Texture width, for texels per screen pixel.
    w: u32,
    full_w: u32,
    full_h: u32,
    orient: u8,
}

impl Tex {
    fn of(path: &Arc<Path>, img: &Img) -> Tex {
        Tex { path: path.clone(), id: img.id, rank: img.rank(), w: img.w, full_w: img.full_w, full_h: img.full_h, orient: img.orient }
    }

    /// Good enough for `img`: these very pixels, or the full image where `img` is a preview.
    fn serves(&self, path: &Arc<Path>, img: &Img) -> bool {
        self.path == *path && (self.id == img.id || self.rank > img.rank())
    }
}

/// The folder being browsed.
pub struct Nav {
    files: Vec<Arc<Path>>,
    cur: usize,
    /// Generation of the listing in flight; older listings are ignored.
    generation: u64,
    /// The file that was opened, to find in the listing when it comes.
    select: Option<Arc<Path>>,
}

impl Nav {
    pub fn empty() -> Nav {
        Nav { files: Vec::new(), cur: 0, generation: 0, select: None }
    }

    /// Start browsing at `p` (a file or a folder): the file's preview is asked for at once, its
    /// folder is listed alongside.
    pub fn open(loader: &Loader, p: PathBuf, generation: u64) -> Nav {
        let p = std::path::absolute(&p).unwrap_or(p);
        let generation = generation + 1;
        if p.is_dir() {
            loader.list(p.into(), generation);
            return Nav { files: Vec::new(), cur: 0, generation, select: None };
        }
        let file: Arc<Path> = p.into();
        loader.plan(vec![(file.clone(), THUMB), (file.clone(), PREVIEW)]);
        if let Some(dir) = file.parent() {
            loader.list(dir.into(), generation);
        }
        Nav { files: vec![file.clone()], cur: 0, generation, select: Some(file) }
    }
}

pub struct App {
    hwnd: HWND,
    gpu: Gpu,
    loader: Loader,
    cfg: Config,
    painter: Painter,
    nav: Nav,
    cache: HashMap<Arc<Path>, Slot>,
    keep: HashSet<Arc<Path>>,
    shown: Option<Tex>,
    /// Which GPU slot holds the shown photo, and what every image slot holds.
    shown_slot: usize,
    slots: [Option<Tex>; IMAGE_SLOTS],
    zoom: f64,
    pan: (f64, f64),
    fit: bool,
    /// The user's quarter turns clockwise, on top of the file's EXIF orientation.
    rot: u8,
    dir: isize,
    size: (u32, u32),
    dpi: u32,
    fs: Fullscreen,
    info: bool,
    bg_mode: u8,
    mouse: (i32, i32),
    drag: Option<(i32, i32)>,
    hot: Option<Button>,
    pressed: Option<Button>,
    file_size: u64,
    dirty: bool,
    chrome_dirty: bool,
    pill_text: String,
    error: Option<String>,
    first_frame: bool,
    traced_image: bool,
    /// When the last flip key went down, for the trace: key to frame on screen.
    flip_at: Option<std::time::Instant>,
    hidden: bool,
    /// Hidden by cloaking (`win::park`) rather than by `ShowWindow`: revealed in 0.1 ms.
    cloaked: bool,
    /// A file came from another launch at this moment: traced until its full pixels show.
    open_at: Option<(std::time::Instant, u8)>,
    setup: Option<Setup>,
    /// Neighbours to put on the GPU once the current frame is out: uploaded in the same frame
    /// as a flip, they made the flip itself three times slower.
    prefetch_pending: bool,
    /// The bottom bar: the pointer near the bottom edge, the button under it, where it was
    /// drawn, and what it was drawn with (redrawn only when that changes).
    bar_hover: bool,
    bar_hot: Option<Tool>,
    bar_at: (i32, i32),
    bar_items: Vec<(Tool, windows_sys::Win32::Foundation::RECT)>,
    bar_key: String,
    /// What the caption on the GPU shows: drawn again only when that changes.
    caption_key: String,
    /// Shown again, the window is maximised if it was when hidden.
    restore_max: bool,
    /// Start this exe with these arguments once the loop is over: the installed copy, after
    /// installing from wherever this one was run.
    pub relaunch: Option<(PathBuf, Vec<std::ffi::OsString>)>,
}

/// The setup card while it is open.
struct Setup {
    /// `None`: this is the installed copy, nothing to install.
    install: Option<bool>,
    autostart: bool,
    default: bool,
    hot: Option<CardItem>,
    note: String,
    /// Where it was drawn last, in client pixels, and its parts in its own.
    at: (i32, i32),
    items: Vec<(CardItem, windows_sys::Win32::Foundation::RECT)>,
    dirty: bool,
}

impl App {
    pub fn new(hwnd: HWND, gpu: Gpu, loader: Loader, cfg: Config, nav: Nav) -> App {
        loader.set_max_dim(gpu.max_dim);
        let size = win::client_size(hwnd);
        let info = cfg.info;
        let mut a = App {
            hwnd,
            gpu,
            loader,
            cfg,
            painter: Painter::new(),
            nav,
            cache: HashMap::new(),
            keep: HashSet::new(),
            shown: None,
            shown_slot: 0,
            slots: Default::default(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            fit: true,
            rot: 0,
            dir: 1,
            size,
            dpi: win::dpi(hwnd),
            fs: Fullscreen::new(),
            info,
            bg_mode: 0,
            mouse: (0, 0),
            drag: None,
            hot: None,
            pressed: None,
            file_size: 0,
            dirty: true,
            chrome_dirty: true,
            pill_text: String::new(),
            error: None,
            first_frame: true,
            traced_image: false,
            flip_at: None,
            hidden: !win::is_visible(hwnd),
            cloaked: false,
            open_at: None,
            setup: None,
            prefetch_pending: false,
            bar_hover: false,
            bar_hot: None,
            bar_at: (0, 0),
            bar_items: Vec::new(),
            bar_key: String::new(),
            caption_key: String::new(),
            restore_max: win::placement(hwnd).1,
            relaunch: None,
        };
        // The tray icon belongs to the process that stays: the resident one.
        if a.cfg.resident {
            win::tray_add(hwnd);
        }
        if !a.cfg.setup_done && !a.hidden {
            a.open_setup(true);
        }
        if a.cfg.window == crate::config::WindowMode::Fullscreen && !a.hidden {
            a.set_fullscreen(true);
        }
        // What already arrived first: planning before it would ask for the same preview again.
        a.keep.extend(a.cur_path());
        a.poll();
        a.cur_changed();
        a
    }

    fn cur_path(&self) -> Option<Arc<Path>> {
        self.nav.files.get(self.nav.cur).cloned()
    }

    fn caption_h(&self) -> i32 {
        if self.fs.on { 0 } else { ui::caption_h(self.dpi) }
    }

    /// The part of the window the photo lives in: under the caption.
    fn area(&self) -> (f64, f64, f64, f64) {
        let top = self.caption_h() as f64;
        (0.0, top, self.size.0 as f64, (self.size.1 as f64 - top).max(1.0))
    }

    /// The photo's size on screen at zoom 1: its full size, turned by EXIF and by the user.
    fn dims(&self) -> Option<(f64, f64)> {
        let t = self.shown.as_ref()?;
        let swap = (t.orient >= 5) ^ (self.rot % 2 == 1);
        let (w, h) = (t.full_w as f64, t.full_h as f64);
        Some(if swap { (h, w) } else { (w, h) })
    }

    fn fit_zoom(&self) -> f64 {
        let Some((w, h)) = self.dims() else { return 1.0 };
        let (_, _, aw, ah) = self.area();
        let z = (aw / w).min(ah / h);
        if self.cfg.upscale_small { z } else { z.min(1.0) }
    }

    fn apply_fit(&mut self) {
        if self.fit {
            self.zoom = self.fit_zoom();
            self.pan = (0.0, 0.0);
        }
        self.clamp_pan();
    }

    /// A photo larger than the window may be pushed until its edge meets the window's edge; a
    /// smaller one stays centred on that axis.
    fn clamp_pan(&mut self) {
        let Some((w, h)) = self.dims() else { return };
        let (_, _, aw, ah) = self.area();
        let lim = |size: f64, a: f64| ((size - a) / 2.0).max(0.0);
        let (lx, ly) = (lim(w * self.zoom, aw), lim(h * self.zoom, ah));
        self.pan = (self.pan.0.clamp(-lx, lx), self.pan.1.clamp(-ly, ly));
    }

    /// Zoom by `factor` keeping the photo's point under (mx, my) where it is. Passing fit or
    /// 100 % stops there once: those two are where one wants to land.
    fn zoom_at(&mut self, factor: f64, mx: f64, my: f64) {
        if self.dims().is_none() {
            return;
        }
        let fit = self.fit_zoom();
        let lo = fit.min(1.0) * 0.5;
        let mut z = (self.zoom * factor).clamp(lo, MAX_ZOOM);
        for t in [fit, 1.0] {
            if (self.zoom < t - 1e-9 && z > t) || (self.zoom > t + 1e-9 && z < t) {
                z = t;
            }
        }
        self.set_zoom(z, mx, my);
    }

    fn set_zoom(&mut self, z: f64, mx: f64, my: f64) {
        let (ax, ay, aw, ah) = self.area();
        let (cx, cy) = (ax + aw / 2.0, ay + ah / 2.0);
        let q = ((mx - cx - self.pan.0) / self.zoom, (my - cy - self.pan.1) / self.zoom);
        self.zoom = z;
        self.pan = (mx - cx - q.0 * z, my - cy - q.1 * z);
        self.fit = (z - self.fit_zoom()).abs() < 1e-9;
        self.clamp_pan();
        self.chrome_dirty = true;
        self.dirty = true;
    }

    fn area_center(&self) -> (f64, f64) {
        let (ax, ay, aw, ah) = self.area();
        (ax + aw / 2.0, ay + ah / 2.0)
    }

    fn step(&mut self, d: isize) {
        let n = self.nav.files.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.nav.cur as isize + d;
        if self.cfg.wrap {
            i = i.rem_euclid(n);
        } else {
            i = i.clamp(0, n - 1);
        }
        self.go(i as usize, d.signum());
    }

    fn go(&mut self, i: usize, dir: isize) {
        if i == self.nav.cur || i >= self.nav.files.len() {
            return;
        }
        self.nav.cur = i;
        if dir != 0 {
            self.dir = dir;
        }
        self.cur_changed();
    }

    fn cur_changed(&mut self) {
        let name = self.cur_path().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned());
        let title = match &name {
            Some(n) => format!("{n} — Glint"),
            None => "Glint".into(),
        };
        win::set_title(self.hwnd, &title);
        self.file_size = self.cur_path().and_then(|p| std::fs::metadata(&*p).ok()).map_or(0, |m| m.len());
        self.schedule();
        self.refresh_texture();
        self.prefetch_pending = true;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// Tell the loader what is wanted now, in order: this image, the next one in the direction
    /// of travel, this image in full, then the rest of the window around it.
    fn schedule(&mut self) {
        let n = self.nav.files.len();
        if n == 0 {
            self.keep.clear();
            drop_later(std::mem::take(&mut self.cache));
            self.loader.plan(Vec::new());
            return;
        }
        let cur = self.nav.cur as isize;
        let at = |d: isize| -> Option<usize> {
            let i = cur + d;
            if self.cfg.wrap { Some(i.rem_euclid(n as isize) as usize) } else { (0..n as isize).contains(&i).then_some(i as usize) }
        };
        let mut order = vec![self.nav.cur];
        for k in 1..=self.cfg.prefetch_ahead.max(self.cfg.prefetch_behind) as isize {
            if k as usize <= self.cfg.prefetch_ahead {
                order.extend(at(k * self.dir));
            }
            if k as usize <= self.cfg.prefetch_behind {
                order.extend(at(-k * self.dir));
            }
        }
        let mut seen = HashSet::new();
        order.retain(|i| seen.insert(*i));
        let paths: Vec<Arc<Path>> = order.iter().map(|&i| self.nav.files[i].clone()).collect();
        self.keep = paths.iter().cloned().collect();
        let keep = &self.keep;
        let gone: Vec<Slot> = self.cache.extract_if(|k, _| !keep.contains(k)).map(|(_, s)| s).collect();
        drop_later(gone);
        // Full-size pixels only for the current image: a neighbour's preview is enough to
        // flip to, and seven full 24-megapixel photos would be 700 MB.
        let current = paths[0].clone();
        let mut freed = Vec::new();
        for (k, s) in self.cache.iter_mut() {
            if *k != current && s.preview.is_some() {
                freed.extend(s.full.take());
            }
        }
        drop_later(freed);

        let mut jobs: Vec<Key> = Vec::new();
        let need_preview = |p: &Arc<Path>| self.cache.get(p).is_none_or(|s| !s.decoded() && s.err.is_none());
        // A stand-in only where nothing is on screen yet for it, and for the next two: flipping
        // fast lands on those before their previews are done.
        let need_thumb = |p: &Arc<Path>| self.cache.get(p).is_none_or(|s| s.best().is_none() && s.err.is_none() && !s.thumb_tried);
        let cur_slot = self.cache.get(&current);
        let cur_full = cur_slot.is_some_and(|s| s.preview.is_some() && s.full.is_none() && s.err.is_none());
        for (k, p) in paths.iter().enumerate() {
            if k <= 2 && need_thumb(p) {
                jobs.push((p.clone(), THUMB));
            }
            if need_preview(p) {
                jobs.push((p.clone(), PREVIEW));
            }
            // After the first neighbour: flipping on is the likelier next move than zooming in.
            if k == 1.min(paths.len() - 1) && cur_full {
                jobs.push((current.clone(), FULL));
            }
        }
        self.loader.plan(jobs);
    }

    fn poll(&mut self) {
        self.take_loaded(None);
    }

    /// The current file has something to show, or never will have a stand-in.
    fn cur_ready(&self) -> bool {
        self.cur_path().and_then(|p| self.cache.get(&p)).is_some_and(|s| s.best().is_some() || s.err.is_some() || s.thumb_tried)
    }

    /// Take what the loader sent. With `until`, wait — up to then — for the current file's
    /// first pixels: the window is about to be revealed, and a frame with the photo in it is
    /// worth a few milliseconds more than an empty one followed by the photo.
    fn take_loaded(&mut self, until: Option<std::time::Instant>) {
        let (mut changed, mut listed) = (false, false);
        loop {
            let m = match until {
                Some(d) if !self.cur_ready() => match d.checked_duration_since(std::time::Instant::now()) {
                    Some(left) => self.loader.rx.recv_timeout(left).ok(),
                    None => self.loader.rx.try_recv().ok(),
                },
                _ => self.loader.rx.try_recv().ok(),
            };
            let Some(m) = m else { break };
            match m {
                Msg::Decoded { path, kind, img } => {
                    if !self.keep.contains(&path) {
                        drop_later(img);
                        continue;
                    }
                    let s = self.cache.entry(path).or_default();
                    match img {
                        // A late stand-in is of no use once real pixels are there.
                        Ok(i) if kind == THUMB => {
                            if s.decoded() {
                                drop_later(i);
                            } else {
                                s.thumb = Some(i);
                            }
                        }
                        Err(_) if kind == THUMB => s.thumb_tried = true,
                        Ok(i) => {
                            if i.full {
                                s.full = Some(i);
                            } else {
                                s.preview = Some(i);
                            }
                            drop_later(s.thumb.take());
                        }
                        Err(e) => s.err = Some(e),
                    }
                    changed = true;
                }
                Msg::Listed { generation, files } => {
                    if generation != self.nav.generation {
                        continue;
                    }
                    let select = self.nav.select.take();
                    self.nav.files = files;
                    self.nav.cur = 0;
                    if let Some(s) = select {
                        match self.nav.files.iter().position(|f| files::same_name(f, &s)) {
                            // THE OPENED PATH, not the listed one: the cache is keyed by it, and
                            // the two can differ in case or spelling.
                            Some(i) => {
                                self.nav.files[i] = s;
                                self.nav.cur = i;
                            }
                            None => self.nav.files.insert(0, s),
                        }
                    }
                    listed = true;
                }
            }
        }
        // PLANNED ONCE, after everything that came: a listing planned before the decoded preview
        // behind it in the channel asked for that same preview a second time.
        if listed {
            self.cur_changed();
        } else if changed {
            self.schedule();
            self.refresh_texture();
            self.prefetch_pending = true;
            self.chrome_dirty = true;
            self.dirty = true;
        }
    }

    /// Put the best pixels there are for the current file on the GPU. Until they come, the
    /// previous photo stays: a black flash on every flip is worse than a moment of the old one.
    fn refresh_texture(&mut self) {
        let Some(path) = self.cur_path() else {
            self.shown = None;
            return;
        };
        let slot = self.cache.get(&path);
        if slot.is_some_and(|s| s.err.is_some() && s.best().is_none()) {
            self.shown = None;
            self.dirty = true;
            return;
        }
        let Some(img) = slot.and_then(|s| s.best()).cloned() else { return };
        if self.shown.as_ref().is_some_and(|t| t.serves(&path, &img)) {
            return;
        }
        let Some((gpu_slot, tex)) = self.on_gpu(&path, &img) else { return };
        self.shown_slot = gpu_slot;
        let new_file = self.shown.as_ref().is_none_or(|t| t.path != path);
        self.shown = Some(tex);
        if new_file && !self.cfg.keep_zoom {
            self.fit = true;
            self.rot = 0;
        }
        self.apply_fit();
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// The GPU slot holding these pixels, uploading them if none does. A slot is taken from,
    /// in order: the same file's older pixels (a preview making way for the full image), a free
    /// one, the photo farthest from the current. The shown photo's slot is never taken.
    fn on_gpu(&mut self, path: &Arc<Path>, img: &Img) -> Option<(usize, Tex)> {
        if let Some(i) = self.slots.iter().position(|s| s.as_ref().is_some_and(|t| t.serves(path, img))) {
            return self.slots[i].clone().map(|t| (i, t));
        }
        let shown = self.shown.is_some().then_some(self.shown_slot);
        let same = self.slots.iter().position(|s| s.as_ref().is_some_and(|t| t.path == *path));
        let free = || self.slots.iter().position(Option::is_none);
        let far = || {
            let n = self.nav.files.len().max(1) as isize;
            let cur = self.nav.cur as isize;
            (0..IMAGE_SLOTS).filter(|&i| Some(i) != shown).max_by_key(|&i| {
                let at = self.slots[i].as_ref().and_then(|t| self.nav.files.iter().position(|f| *f == t.path));
                at.map_or(isize::MAX, |a| {
                    let d = (a as isize - cur).rem_euclid(n);
                    d.min(n - d)
                })
            })
        };
        let i = same.or_else(free).or_else(far)?;
        if let Err(e) = self.gpu.upload_image(i, img.w, img.h, &img.px, !img.thumb) {
            self.error = Some(e);
            return None;
        }
        let tex = Tex::of(path, img);
        self.slots[i] = Some(tex.clone());
        // Drawn at once, so the upload is done by the time the user flips to it.
        self.dirty = true;
        Some((i, tex))
    }

    /// Put the neighbours that are decoded on the GPU ahead of the flip: the next one in the
    /// direction of travel, the previous one, and the one after next.
    fn prefetch_gpu(&mut self) {
        let n = self.nav.files.len();
        if n < 2 {
            return;
        }
        for d in [self.dir, -self.dir, 2 * self.dir] {
            let i = self.nav.cur as isize + d;
            let i = if self.cfg.wrap { i.rem_euclid(n as isize) as usize } else if (0..n as isize).contains(&i) { i as usize } else { continue };
            let path = self.nav.files[i].clone();
            if let Some(img) = self.cache.get(&path).and_then(|s| s.best()).cloned() {
                self.on_gpu(&path, &img);
            }
        }
    }

    fn open(&mut self, p: PathBuf) {
        self.nav = Nav::open(&self.loader, p, self.nav.generation);
        self.cur_changed();
    }

    /// Shown again for a new file: the window comes back as the settings say.
    fn show(&mut self) {
        if !self.hidden {
            win::focus(self.hwnd);
            return;
        }
        self.hidden = false;
        if std::mem::take(&mut self.cloaked) {
            // The frame is there before the window is. (The caption was drawn when the file
            // came, while its thumbnail was looked up.)
            self.dirty = true;
            let t = std::time::Instant::now();
            win::Handler::frame(self);
            crate::trace(&format!("  [show] frame {:.2}", t.elapsed().as_secs_f64() * 1e3));
            win::reveal(self.hwnd);
            crate::trace(&format!("  [show] reveal {:.2}", t.elapsed().as_secs_f64() * 1e3));
            if let Some((t, _)) = self.open_at {
                let what = match self.shown.as_ref().filter(|s| Some(&s.path) == self.cur_path().as_ref()) {
                    Some(s) => ["thumbnail", "preview", "full image"][s.rank as usize],
                    None => "nothing yet",
                };
                crate::trace(&format!("open -> revealed with {what} {:.2} ms", t.elapsed().as_secs_f64() * 1000.0));
            }
        } else {
            win::show(self.hwnd, self.restore_max);
        }
        if self.cfg.window == crate::config::WindowMode::Fullscreen {
            self.set_fullscreen(true);
        }
        self.chrome_dirty = true;
        self.dirty = true;
    }

    /// Closed while resident: everything the photos held goes — decoded pixels, textures, the
    /// staging buffer — and the working set is trimmed. The device, swapchain and pipeline
    /// stay: those are what makes the next open instant.
    /// Remember where the window is, for the next start.
    fn save_placement(&mut self) {
        if self.hidden {
            return;
        }
        self.set_fullscreen(false);
        let (r, max) = win::placement(self.hwnd);
        self.restore_max = max;
        self.cfg.placement = Some((r, max));
        crate::config::store("placement", &format!("{},{},{},{},{}", r[0], r[1], r[2], r[3], max as u8));
    }

    fn hide(&mut self) {
        self.save_placement();
        self.nav = Nav { generation: self.nav.generation, ..Nav::empty() };
        self.loader.plan(Vec::new());
        drop_later(std::mem::take(&mut self.cache));
        self.keep.clear();
        self.shown = None;
        for i in 0..IMAGE_SLOTS {
            self.gpu.clear(i);
        }
        self.slots = Default::default();
        self.gpu.clear(SLOT_INFO);
        self.gpu.clear(SLOT_CARD);
        // The toolbar stays: the same next time, and drawing it again was a millisecond of
        // the next open.
        self.setup = None;
        self.pill_text.clear();
        self.error = None;
        // One empty frame before hiding: shown again, the window must not flash the old photo
        // before its first new frame.
        self.chrome_dirty = true;
        self.dirty = true;
        win::set_title(self.hwnd, "Glint");
        win::Handler::frame(self);
        self.park();
        self.gpu.release_staging();
        self.settle();
    }

    /// Hidden, with nothing to do until the next open: memory given back, then what that
    /// open's first frame needs brought back in.
    pub fn settle(&mut self) {
        if self.cfg.trim_memory {
            win::trim_memory();
        }
        self.gpu.warm();

    }

    /// Out of sight, ready to be revealed: see `win::park`.
    pub fn park(&mut self) {
        win::park(self.hwnd, self.restore_max);
        self.hidden = true;
        self.cloaked = true;
    }

    fn set_fullscreen(&mut self, on: bool) {
        self.fs.set(self.hwnd, on);
        self.hot = None;
        self.chrome_dirty = true;
        self.dirty = true;
    }

    fn key(&mut self, vk: u16, repeat: bool) {
        if self.setup.is_some() {
            match vk {
                VK_ESCAPE => self.close_setup(),
                VK_RETURN => self.apply_setup(),
                _ => {}
            }
            return;
        }
        let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
        let (cx, cy) = self.area_center();
        match vk {
            VK_RIGHT | VK_NEXT | VK_SPACE => self.step(1),
            VK_LEFT | VK_PRIOR | VK_BACK => self.step(-1),
            VK_HOME => self.go(0, -1),
            VK_END => self.go(self.nav.files.len().saturating_sub(1), 1),
            VK_UP | VK_ADD | VK_OEM_PLUS => self.zoom_at(STEP, cx, cy),
            VK_DOWN | VK_SUBTRACT | VK_OEM_MINUS => self.zoom_at(1.0 / STEP, cx, cy),
            _ if repeat => {}
            0x30 | VK_NUMPAD0 => {
                self.fit = true;
                self.apply_fit();
                self.chrome_dirty = true;
                self.dirty = true;
            }
            0x31 | VK_NUMPAD1 => self.set_zoom(1.0, cx, cy),
            0x46 | VK_F11 | VK_RETURN => self.set_fullscreen(!self.fs.on),
            VK_ESCAPE => {
                if self.fs.on {
                    self.set_fullscreen(false)
                } else {
                    win::close(self.hwnd)
                }
            }
            0x52 | 0x4C => {
                // R turns clockwise, Shift+R or L back.
                let back = shift || vk == 0x4C;
                self.rot = (self.rot + if back { 3 } else { 1 }) % 4;
                self.fit = true;
                self.apply_fit();
                self.chrome_dirty = true;
                self.dirty = true;
            }
            0x49 | VK_TAB => {
                self.info = !self.info;
                self.dirty = true;
            }
            0x42 => {
                self.bg_mode = (self.bg_mode + 1) % 4;
                self.dirty = true;
            }
            VK_DELETE => self.delete(),
            0x4F => {
                if let Some(p) = files::open_dialog(self.hwnd as isize) {
                    self.open(p);
                }
            }
            0x51 => win::close(self.hwnd),
            _ => {}
        }
    }

    fn delete(&mut self) {
        let Some(p) = self.cur_path() else { return };
        if !files::recycle(self.hwnd as isize, &p) {
            return;
        }
        self.nav.files.remove(self.nav.cur);
        self.cache.remove(&p);
        if self.shown.as_ref().is_some_and(|t| t.path == p) {
            self.shown = None;
        }
        for i in 0..IMAGE_SLOTS {
            if self.slots[i].as_ref().is_some_and(|t| t.path == p) {
                self.slots[i] = None;
                self.gpu.clear(i);
            }
        }
        if self.nav.cur >= self.nav.files.len() {
            self.nav.cur = self.nav.files.len().saturating_sub(1);
        }
        self.cur_changed();
    }

    /// Open the card; on the first start both boxes are ticked, later they show what is set.
    fn open_setup(&mut self, first: bool) {
        self.setup = Some(Setup {
            install: (!crate::install::is_installed_copy()).then_some(first),
            autostart: first || crate::assoc::autostart(),
            default: first || crate::assoc::is_default(),
            hot: None,
            note: String::new(),
            at: (0, 0),
            items: Vec::new(),
            dirty: true,
        });
        self.dirty = true;
    }

    fn close_setup(&mut self) {
        self.setup = None;
        self.gpu.clear(SLOT_CARD);
        crate::config::store("setup_done", "true");
        self.cfg.setup_done = true;
        self.dirty = true;
    }

    fn apply_setup(&mut self) {
        let Some(s) = self.setup.as_mut() else { return };
        let mut notes = Vec::new();
        let mut exe = std::env::current_exe().unwrap_or_default();
        let mut installed = false;
        if s.install == Some(true) {
            match crate::install::install() {
                Ok(p) => {
                    exe = p;
                    installed = true;
                }
                Err(e) => notes.push(format!("install: {e}")),
            }
        }
        if let Err(e) = crate::assoc::set_autostart(s.autostart, &exe) {
            notes.push(e);
        }
        match crate::assoc::set_default(s.default, &exe) {
            Ok(false) if s.default => notes.push("Windows asks to confirm in Settings".to_string()),
            Err(e) => notes.push(e),
            _ => {}
        }
        // Autostart means resident: the process that starts at login is the one photos open in.
        if s.autostart && !self.cfg.resident {
            self.cfg.resident = true;
            crate::config::store("resident", "true");
            win::set_resident(true);
        }
        if installed {
            // THE INSTALLED COPY TAKES OVER, on the same photo and at the same place: this one
            // closes first, so the new one does not hand the file back to it.
            let (r, max) = if self.hidden { self.cfg.placement.unwrap_or(([0; 4], true)) } else { win::placement(self.hwnd) };
            let ini = crate::install::dir().join("glint.ini");
            crate::config::store_at(&ini, "placement", &format!("{},{},{},{},{}", r[0], r[1], r[2], r[3], max as u8));
            if s.autostart {
                crate::config::store_at(&ini, "resident", "true");
            }
            let args = self.cur_path().map_or_else(Vec::new, |p| vec![p.as_os_str().to_owned()]);
            self.relaunch = Some((exe, args));
            self.setup = None;
            win::destroy(self.hwnd);
            return;
        }
        if notes.is_empty() {
            self.close_setup();
        } else {
            s.note = notes.join("; ");
            s.dirty = true;
            crate::config::store("setup_done", "true");
            self.dirty = true;
        }
    }

    /// The card item under a client point.
    fn card_item_at(&self, x: i32, y: i32) -> Option<CardItem> {
        let s = self.setup.as_ref()?;
        let (px, py) = (x - s.at.0, y - s.at.1);
        s.items.iter().find(|(_, r)| px >= r.left && px < r.right && py >= r.top && py < r.bottom).map(|(i, _)| *i)
    }

    fn card_click(&mut self, item: CardItem) {
        let Some(s) = self.setup.as_mut() else { return };
        match item {
            CardItem::Install => s.install = s.install.map(|v| !v),
            CardItem::Autostart => s.autostart = !s.autostart,
            CardItem::Default => s.default = !s.default,
            CardItem::Apply => return self.apply_setup(),
            CardItem::Skip => return self.close_setup(),
        }
        s.dirty = true;
        self.dirty = true;
    }

    /// The bar shows by itself while the photo is fitted in a window; zoomed in or fullscreen it
    /// keeps out of the way until the pointer comes near the bottom.
    fn bar_visible(&self) -> bool {
        // Hidden only while the photo is zoomed in past the window: zoomed out it stays, since
        // then it covers nothing.
        let zoomed_in = self.zoom > self.fit_zoom() + 1e-9;
        self.setup.is_none() && !self.nav.files.is_empty() && (self.bar_hover || (!self.fs.on && !zoomed_in))
    }

    fn in_bar_zone(&self, y: i32) -> bool {
        y >= self.size.1 as i32 - ui::scaled(96, self.dpi)
    }

    fn tool_at(&self, x: i32, y: i32) -> Option<Tool> {
        if !self.bar_visible() || !self.gpu.has(SLOT_BAR) {
            return None;
        }
        let (px, py) = (x - self.bar_at.0, y - self.bar_at.1);
        self.bar_items.iter().find(|(_, r)| px >= r.left && px < r.right && py >= r.top && py < r.bottom).map(|(t, _)| *t)
    }

    fn tool(&mut self, t: Tool) {
        let (cx, cy) = self.area_center();
        match t {
            Tool::Prev => self.step(-1),
            Tool::Next => self.step(1),
            Tool::ZoomOut => self.zoom_at(1.0 / STEP, cx, cy),
            Tool::ZoomIn => self.zoom_at(STEP, cx, cy),
            Tool::Fit => {
                self.fit = true;
                self.apply_fit();
                self.chrome_dirty = true;
            }
            Tool::Actual => self.set_zoom(1.0, cx, cy),
            Tool::Rotate => {
                self.rot = (self.rot + 1) % 4;
                self.fit = true;
                self.apply_fit();
                self.chrome_dirty = true;
            }
            Tool::Fullscreen => self.set_fullscreen(!self.fs.on),
            Tool::Delete => self.delete(),
        }
        self.dirty = true;
    }

    fn button(&mut self, b: Button) {
        match b {
            Button::Settings => {
                if self.setup.is_some() {
                    self.close_setup();
                } else {
                    self.open_setup(false);
                }
            }
            Button::Min => win::minimize(self.hwnd),
            Button::Max => win::toggle_maximize(self.hwnd),
            Button::Close => win::close(self.hwnd),
        }
    }

    fn hot_at(&self, x: i32, y: i32) -> Option<Button> {
        if self.fs.on { None } else { ui::button_at(x, y, self.size.0 as i32, self.dpi) }
    }

    fn info_line(&self) -> String {
        let n = self.nav.files.len();
        if n == 0 {
            return String::new();
        }
        let count = if self.nav.select.is_some() { format!("{} / …", self.nav.cur + 1) } else { format!("{} / {}", self.nav.cur + 1, n) };
        let mut parts = vec![count];
        let current = self.cur_path();
        match self.shown.as_ref().filter(|t| Some(&t.path) == current.as_ref()) {
            Some(t) => {
                parts.push(format!("{} × {}", t.full_w, t.full_h));
                parts.push(format!("{:.0}%", self.zoom * 100.0));
            }
            None => parts.push("…".into()),
        }
        if self.file_size > 0 {
            let mb = self.file_size as f64 / (1024.0 * 1024.0);
            parts.push(if mb >= 1.0 { format!("{mb:.1} MB") } else { format!("{:.0} KB", self.file_size as f64 / 1024.0) });
        }
        parts.join("    ")
    }

    fn background(&self) -> [f32; 3] {
        match self.bg_mode {
            1 => [0.0; 3],
            2 => [0.93; 3],
            _ => self.cfg.background,
        }
    }

    /// What the pill in the middle or at the bottom says, if anything.
    fn pill(&self) -> Option<(String, bool)> {
        if let Some(e) = &self.error {
            return Some((format!("GPU: {e}"), true));
        }
        let Some(p) = self.cur_path() else {
            return Some(("Drop an image here  ·  O — open a file".into(), true));
        };
        if let Some(e) = self.cache.get(&p).and_then(|s| s.err.as_ref()).filter(|_| self.shown.is_none()) {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            return Some((format!("Cannot open {name}: {e}"), true));
        }
        if self.fs.on && self.info {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            return Some((format!("{name}    {}", self.info_line()), false));
        }
        None
    }

    fn quads(&self, pill: Option<(bool, (u32, u32))>) -> Vec<(usize, Quad)> {
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        let ndc = |x0: f64, y0: f64, x1: f64, y1: f64| [(x0 / w * 2.0 - 1.0) as f32, (y0 / h * 2.0 - 1.0) as f32, (x1 / w * 2.0 - 1.0) as f32, (y1 / h * 2.0 - 1.0) as f32];
        let flat = |rect| Quad { rect, m: [1.0, 0.0, 0.0, 1.0], o: [0.0, 0.0, 1.0, 1.0], bg: [0.0; 4] };
        let mut out = Vec::new();
        if let (Some(img), Some((dw, dh))) = (&self.shown, self.dims()) {
            let (cx, cy) = self.area_center();
            let (sw, sh) = (dw * self.zoom, dh * self.zoom);
            // Whole-pixel corner: at 100 % every texel lands on one pixel, not between two.
            let (x0, y0) = ((cx + self.pan.0 - sw / 2.0).round(), (cy + self.pan.1 - sh / 2.0).round());
            let (m, o) = uv_transform(img.orient, self.rot);
            let bg = self.background();
            out.push((self.shown_slot, Quad {
                rect: ndc(x0, y0, x0 + sw, y0 + sh),
                m,
                o: [o[0], o[1], (self.zoom * img.full_w as f64 / img.w as f64) as f32, 0.0],
                bg: [bg[0], bg[1], bg[2], if self.bg_mode == 3 { 1.0 } else { 0.0 }],
            }));
        }
        let cap = self.caption_h() as f64;
        if cap > 0.0 {
            out.push((SLOT_CAPTION, flat(ndc(0.0, 0.0, w, cap))));
        }
        if let Some((center, (pw, ph))) = pill {
            let (pw, ph) = (pw as f64, ph as f64);
            let x = ((w - pw) / 2.0).round();
            let y = if center { (cap + (h - cap - ph) / 2.0).round() } else { cap + ui::scaled(16, self.dpi) as f64 };
            out.push((SLOT_INFO, flat(ndc(x, y, x + pw, y + ph))));
        }
        out
    }
}

/// The corner → uv map for an EXIF orientation and `rot` user quarter turns clockwise, as the
/// column-major matrix and offset the vertex shader takes. uv = M·c + o.
fn uv_transform(orient: u8, rot: u8) -> ([f32; 4], [f32; 2]) {
    // Rows: uv.x = m[0]·c, uv.y = m[1]·c. EXIF says how the stored image is turned for display.
    let (mut m, mut o): ([[i32; 2]; 2], [i32; 2]) = match orient {
        2 => ([[-1, 0], [0, 1]], [1, 0]),
        3 => ([[-1, 0], [0, -1]], [1, 1]),
        4 => ([[1, 0], [0, -1]], [0, 1]),
        5 => ([[0, 1], [1, 0]], [0, 0]),
        6 => ([[0, 1], [-1, 0]], [0, 1]),
        7 => ([[0, -1], [-1, 0]], [1, 1]),
        8 => ([[0, -1], [1, 0]], [1, 0]),
        _ => ([[1, 0], [0, 1]], [0, 0]),
    };
    // A clockwise quarter turn of the display: the old corner is c_old = R·c + r.
    let (r, rv) = ([[0, 1], [-1, 0]], [0, 1]);
    for _ in 0..rot % 4 {
        let mr = [
            [m[0][0] * r[0][0] + m[0][1] * r[1][0], m[0][0] * r[0][1] + m[0][1] * r[1][1]],
            [m[1][0] * r[0][0] + m[1][1] * r[1][0], m[1][0] * r[0][1] + m[1][1] * r[1][1]],
        ];
        o = [m[0][0] * rv[0] + m[0][1] * rv[1] + o[0], m[1][0] * rv[0] + m[1][1] * rv[1] + o[1]];
        m = mr;
    }
    ([m[0][0] as f32, m[1][0] as f32, m[0][1] as f32, m[1][1] as f32], [o[0] as f32, o[1] as f32])
}

impl App {
    /// The caption, redrawn and sent up if it changed.
    fn render_chrome(&mut self) {
        let (w, _) = self.size;
        win::set_caption(self.caption_h(), if self.fs.on { 0 } else { ui::buttons_w(self.dpi) });
        if self.chrome_dirty && !self.fs.on && w > 0 {
            let title = self.cur_path().map_or_else(|| "Glint".to_string(), |p| p.file_name().unwrap_or_default().to_string_lossy().into_owned());
            let info = self.info_line();
            let (pressed, max) = (self.pressed.is_some(), win::is_maximized(self.hwnd));
            let key = format!("{title}|{info}|{:?}|{pressed}|{max}|{w}|{}", self.hot, self.dpi);
            if key != self.caption_key || !self.gpu.has(SLOT_CAPTION) {
                let px = self.painter.caption(self.dpi, w as i32, &title, &info, self.hot, pressed, max);
                match self.gpu.upload(SLOT_CAPTION, w, ui::caption_h(self.dpi) as u32, &px) {
                    Ok(()) => self.caption_key = key,
                    Err(e) => self.error = Some(e),
                }
            }
        }
        self.chrome_dirty = false;
    }
}

impl win::Handler for App {
    fn event(&mut self, e: Ev) {
        match e {
            Ev::Resize(w, h) => {
                self.size = (w, h);
                // A hidden window shown again reports the size it already had; rebuilding the
                // swapchain for it cost milliseconds on the way to the first frame.
                if self.gpu.extent.width != w || self.gpu.extent.height != h {
                    self.gpu.resize(w, h);
                }
                let (mw, mh) = win::monitor_size(self.hwnd);
                self.loader.set_target(mw, mh);
                self.apply_fit();
                self.chrome_dirty = true;
                self.dirty = true;
            }
            Ev::Paint => self.dirty = true,
            Ev::Dpi(d) => {
                self.dpi = d;
                self.chrome_dirty = true;
                self.dirty = true;
            }
            Ev::Loaded => self.poll(),
            Ev::Key { vk, down: true, repeat } => {
                self.flip_at = Some(std::time::Instant::now());
                self.key(vk, repeat);
            }
            Ev::Key { .. } => {}
            Ev::MouseMove(x, y) if self.setup.is_some() => {
                let hot = self.card_item_at(x, y);
                let caption_hot = self.hot_at(x, y);
                if let Some(s) = self.setup.as_mut()
                    && s.hot != hot
                {
                    s.hot = hot;
                    s.dirty = true;
                    self.dirty = true;
                }
                if caption_hot != self.hot {
                    self.hot = caption_hot;
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
            }
            Ev::Button { b: Btn::Left, down: true, x, y } if self.setup.is_some() && self.hot_at(x, y).is_none() => {
                if let Some(item) = self.card_item_at(x, y) {
                    self.card_click(item);
                }
            }
            Ev::Wheel { .. } if self.setup.is_some() => {}
            Ev::MouseMove(x, y) => {
                let hover = self.drag.is_none() && self.in_bar_zone(y);
                if hover != self.bar_hover {
                    self.bar_hover = hover;
                    self.dirty = true;
                }
                let tool = self.tool_at(x, y);
                if tool != self.bar_hot {
                    self.bar_hot = tool;
                    self.dirty = true;
                }
                if let Some((px, py)) = self.drag {
                    self.pan.0 += (x - px) as f64;
                    self.pan.1 += (y - py) as f64;
                    self.clamp_pan();
                    self.drag = Some((x, y));
                    self.dirty = true;
                }
                self.mouse = (x, y);
                let hot = self.hot_at(x, y);
                if hot != self.hot {
                    self.hot = hot;
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
            }
            Ev::MouseLeave => {
                if self.bar_hover || self.bar_hot.is_some() {
                    self.bar_hover = false;
                    self.bar_hot = None;
                    self.dirty = true;
                }
                if self.hot.is_some() && self.pressed.is_none() {
                    self.hot = None;
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
            }
            Ev::Button { b: Btn::Left, down: true, x, y } if self.tool_at(x, y).is_some() => {
                if let Some(t) = self.tool_at(x, y) {
                    self.tool(t);
                }
            }
            Ev::Button { b: Btn::Left, down: true, x, y } => match self.hot_at(x, y) {
                Some(b) => {
                    self.pressed = Some(b);
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
                None => {
                    self.drag = Some((x, y));
                    win::set_cursor(win::IDC_SIZEALL);
                }
            },
            Ev::Button { b: Btn::Left, down: false, x, y } => {
                if let Some(b) = self.pressed.take() {
                    self.chrome_dirty = true;
                    self.dirty = true;
                    if self.hot_at(x, y) == Some(b) {
                        self.button(b);
                    }
                }
                if self.drag.take().is_some() {
                    win::set_cursor(win::IDC_ARROW);
                }
            }
            Ev::Button { b: Btn::Back, down: true, .. } => self.step(-1),
            Ev::Button { b: Btn::Forward, down: true, .. } => self.step(1),
            Ev::Button { b: Btn::Middle, down: true, x, y } => {
                if self.fit {
                    self.set_zoom(1.0, x as f64, y as f64);
                } else {
                    self.fit = true;
                    self.apply_fit();
                    self.chrome_dirty = true;
                    self.dirty = true;
                }
            }
            Ev::Button { .. } => {}
            // THE SECOND PRESS OF A QUICK DOUBLE CLICK IS STILL A PRESS for everything but the
            // photo: the window class asks for double clicks, so Windows sends that press as
            // `WM_LBUTTONDBLCLK`, and a checkbox or a button clicked fast lost every other click.
            Ev::DoubleClick(x, y) if self.setup.is_some() || self.tool_at(x, y).is_some() || self.hot_at(x, y).is_some() => {
                self.event(Ev::Button { b: Btn::Left, down: true, x, y });
            }
            Ev::DoubleClick(..) => self.set_fullscreen(!self.fs.on),
            Ev::Wheel { notches, x, y } => {
                let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
                if self.cfg.wheel_zoom != ctrl {
                    self.zoom_at(STEP.powf(notches as f64), x as f64, y as f64);
                } else if notches != 0.0 {
                    self.step(if notches > 0.0 { -1 } else { 1 });
                }
            }
            Ev::Drop(p) => self.open(p),
            Ev::Open(p) => {
                let t = std::time::Instant::now();
                self.open_at = Some((t, 0));
                crate::gpu::TRACE.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(p) = p {
                    self.open(p);
                }
                let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3;
                crate::trace(&format!("  [open] opened {:.2} (plan sent)", ms(t)));
                if self.hidden {
                    // The caption while the thumbnail is looked up on a worker, then the
                    // thumbnail itself if it comes in time.
                    self.chrome_dirty = true;
                    self.render_chrome();
                    crate::trace(&format!("  [open] chrome {:.2}", ms(t)));
                    self.take_loaded(Some(t + std::time::Duration::from_millis(8)));
                    crate::trace(&format!("  [open] waited {:.2} ready={}", ms(t), self.cur_ready()));
                }
                self.show();
            }
            Ev::Close => {
                if self.cfg.resident {
                    self.hide();
                } else {
                    self.save_placement();
                    win::tray_remove(self.hwnd);
                    win::destroy(self.hwnd);
                }
            }
            Ev::Quit => {
                self.save_placement();
                win::tray_remove(self.hwnd);
                win::destroy(self.hwnd);
            }
            Ev::Tray(cmd) => {
                self.open_at = Some((std::time::Instant::now(), 0));
                match cmd {
                    // Whatever is on the clipboard; nothing there — the empty viewer, or the
                    // window as it is.
                    win::TrayCmd::Open => {
                        if let Some(p) = crate::clipboard::image(self.hwnd as isize) {
                            self.open(p);
                        }
                        self.show();
                    }
                    win::TrayCmd::OpenFile => {
                        if let Some(p) = files::open_dialog(self.hwnd as isize) {
                            self.open(p);
                            self.show();
                        }
                    }
                    win::TrayCmd::Settings => {
                        self.show();
                        self.open_setup(false);
                    }
                    win::TrayCmd::Exit => {
                        self.save_placement();
                        win::tray_remove(self.hwnd);
                        win::destroy(self.hwnd);
                    }
                }
            }
        }
    }

    fn frame(&mut self) {
        if !self.dirty || self.hidden {
            return;
        }
        let tf = std::time::Instant::now();
        let tr = |what: &str| {
            if crate::gpu::TRACE.load(std::sync::atomic::Ordering::Relaxed) {
                crate::trace(&format!("    [frame] {what} {:.2}", tf.elapsed().as_secs_f64() * 1e3));
            }
        };
        self.render_chrome();
        tr("chrome");
        let pill = self.pill();
        let pill_geom = match &pill {
            Some((text, center)) => {
                if *text != self.pill_text || !self.gpu.has(SLOT_INFO) {
                    let (pw, ph, px) = self.painter.pill(self.dpi, text);
                    let _ = self.gpu.upload(SLOT_INFO, pw, ph, &px);
                    self.pill_text = text.clone();
                }
                Some((*center, self.gpu.size_of(SLOT_INFO)))
            }
            None => None,
        };
        tr("pill");
        let mut quads = self.quads(pill_geom);
        if self.bar_visible() {
            let zoom = if self.shown.is_some() { format!("{:.0}%", self.zoom * 100.0) } else { String::new() };
            let key = format!("{:?}|{}|{}|{}", self.bar_hot, self.fs.on, zoom, self.dpi);
            if key != self.bar_key || !self.gpu.has(SLOT_BAR) {
                let bar = self.painter.toolbar(self.dpi, self.bar_hot, self.fs.on, &zoom);
                let _ = self.gpu.upload(SLOT_BAR, bar.w, bar.h, &bar.px);
                self.bar_items = bar.items;
                self.bar_key = key;
            }
            let (bw, bh) = self.gpu.size_of(SLOT_BAR);
            let (w, h) = (self.size.0 as f64, self.size.1 as f64);
            let x = ((w - bw as f64) / 2.0).round();
            let y = h - bh as f64 - ui::scaled(16, self.dpi) as f64;
            self.bar_at = (x as i32, y as i32);
            let ndc = |x0: f64, y0: f64, x1: f64, y1: f64| [(x0 / w * 2.0 - 1.0) as f32, (y0 / h * 2.0 - 1.0) as f32, (x1 / w * 2.0 - 1.0) as f32, (y1 / h * 2.0 - 1.0) as f32];
            quads.push((SLOT_BAR, Quad { rect: ndc(x, y, x + bw as f64, y + bh as f64), m: [1.0, 0.0, 0.0, 1.0], o: [0.0, 0.0, 1.0, 1.0], bg: [0.0; 4] }));
        }
        tr("bar");
        let top = self.caption_h() as f64;
        if let Some(s) = self.setup.as_mut() {
            if s.dirty || !self.gpu.has(SLOT_CARD) {
                let mut rows = Vec::new();
                if let Some(v) = s.install {
                    rows.push((CardItem::Install, v));
                }
                rows.push((CardItem::Autostart, s.autostart));
                rows.push((CardItem::Default, s.default));
                let card = self.painter.card(self.dpi, &rows, s.hot, &s.note);
                let _ = self.gpu.upload(SLOT_CARD, card.w, card.h, &card.px);
                s.items = card.items;
                s.dirty = false;
            }
            let (cw, ch) = self.gpu.size_of(SLOT_CARD);
            let (w, h) = (self.size.0 as f64, self.size.1 as f64);
            let x = ((w - cw as f64) / 2.0).round();
            let y = (top + (h - top - ch as f64) / 2.0).round().max(top);
            s.at = (x as i32, y as i32);
            let ndc = |x0: f64, y0: f64, x1: f64, y1: f64| [(x0 / w * 2.0 - 1.0) as f32, (y0 / h * 2.0 - 1.0) as f32, (x1 / w * 2.0 - 1.0) as f32, (y1 / h * 2.0 - 1.0) as f32];
            quads.push((SLOT_CAPTION, Quad { rect: ndc(0.0, top, w, h), m: [1.0, 0.0, 0.0, 1.0], o: [0.0, 0.0, 1.0, 2.0], bg: [0.0, 0.0, 0.0, 0.55] }));
            quads.push((SLOT_CARD, Quad { rect: ndc(x, y, x + cw as f64, y + ch as f64), m: [1.0, 0.0, 0.0, 1.0], o: [0.0, 0.0, 1.0, 1.0], bg: [0.0; 4] }));
        }
        let clear = if self.bg_mode == 3 { self.cfg.background } else { self.background() };
        match self.gpu.draw(clear, &quads) {
            Ok(true) => {
                self.dirty = false;
                if let Some(t) = self.flip_at.take() {
                    let ready = self.shown.as_ref().is_some_and(|t| Some(&t.path) == self.cur_path().as_ref());
                    crate::trace(&format!("key -> frame {:.2} ms{}", t.elapsed().as_secs_f64() * 1000.0, if ready { "" } else { " (image not decoded yet)" }));
                }
                if std::mem::take(&mut self.prefetch_pending) {
                    self.prefetch_gpu();
                    if self.dirty {
                        // The uploads go with a frame of their own, asked for through the queue so
                        // input that came meanwhile is handled first.
                        // SAFETY: posting to our own window.
                        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(self.hwnd, crate::loader::WM_LOADED, 0, 0) };
                    }
                }
                if let Some((t, stage)) = self.open_at {
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    let rank = self.shown.as_ref().filter(|s| Some(&s.path) == self.cur_path().as_ref()).map(|s| s.rank + 1).unwrap_or(0);
                    if stage == 0 {
                        crate::trace(&format!("open -> window {ms:.2} ms"));
                        self.open_at = Some((t, 1));
                    }
                    if rank > stage.max(1) - 1 && rank > 0 && (stage == 0 || rank + 1 > stage) {
                        crate::trace(&format!("open -> {} {ms:.2} ms", ["thumbnail", "preview", "full"][rank as usize - 1]));
                        self.open_at = if rank >= 3 || ms > 3000.0 { None } else { Some((t, rank + 1)) };
                    }
                }
                if self.first_frame {
                    self.first_frame = false;
                    win::set_ready();
                    crate::trace("first frame");
                }
                if self.shown.is_some() && !self.traced_image {
                    self.traced_image = true;
                    crate::trace("first frame with the photo");
                }
            }
            Ok(false) => {}
            Err(e) => {
                self.error = Some(e);
                self.dirty = false;
            }
        }
    }
}
