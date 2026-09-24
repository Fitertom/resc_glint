//! Decoding threads. The main thread says, after every move, what it wants and in which order;
//! workers take jobs from the front. A job no longer wanted is dropped before it starts, and a
//! full decode already running stops at its next band.

use crate::wic::{Img, Wic};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Posted to the window when something arrives, so the loop wakes up without polling.
pub const WM_LOADED: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// What is asked of a file: a stand-in thumbnail, a screen-sized preview, the full image. Each
/// is its own job, so a thumbnail and a preview of one file run side by side.
pub const THUMB: u8 = 0;
pub const PREVIEW: u8 = 1;
pub const FULL: u8 = 2;

pub type Key = (Arc<Path>, u8);

pub enum Msg {
    Decoded { path: Arc<Path>, kind: u8, img: Result<Arc<Img>, String> },
    /// A folder listed: its images, and the listing's generation (older ones are ignored).
    Listed { generation: u64, files: Vec<Arc<Path>> },
}

struct Shared {
    queue: Mutex<VecDeque<Key>>,
    cv: Condvar,
    wanted: Mutex<HashSet<Key>>,
    running: Mutex<HashSet<Key>>,
    hwnd: AtomicIsize,
    /// Screen size for previews, packed w << 32 | h.
    target: AtomicU64,
    max_dim: AtomicU32,
}

pub struct Loader {
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
}

impl Loader {
    pub fn start(target: (u32, u32)) -> Loader {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            wanted: Mutex::new(HashSet::new()),
            running: Mutex::new(HashSet::new()),
            hwnd: AtomicIsize::new(0),
            target: AtomicU64::new((target.0 as u64) << 32 | target.1 as u64),
            max_dim: AtomicU32::new(16384),
        });
        let (tx, rx) = channel();
        // WIC's JPEG decoder is single-threaded: parallelism comes from decoding several files
        // at once. One core is left to the UI thread.
        let n = std::thread::available_parallelism().map_or(4, |n| n.get()).saturating_sub(1).clamp(2, 6);
        for _ in 0..n {
            let (s, tx) = (shared.clone(), tx.clone());
            std::thread::spawn(move || worker(s, tx));
        }
        Loader { shared, tx, rx }
    }

    pub fn set_hwnd(&self, hwnd: isize) {
        self.shared.hwnd.store(hwnd, Ordering::Release);
    }

    pub fn set_max_dim(&self, d: u32) {
        self.shared.max_dim.store(d, Ordering::Relaxed);
    }

    pub fn set_target(&self, w: u32, h: u32) {
        self.shared.target.store((w as u64) << 32 | h as u64, Ordering::Relaxed);
    }

    /// Replace the plan: `jobs` in priority order. Everything not in it is no longer wanted —
    /// queued jobs vanish, running full decodes stop.
    pub fn plan(&self, jobs: Vec<Key>) {
        let running = self.shared.running.lock().unwrap().clone();
        *self.shared.wanted.lock().unwrap() = jobs.iter().cloned().collect();
        let mut q = self.shared.queue.lock().unwrap();
        q.clear();
        q.extend(jobs.into_iter().filter(|k| !running.contains(k)));
        drop(q);
        self.shared.cv.notify_all();
    }

    /// List a folder off the UI thread: a folder of ten thousand files takes a while, and the
    /// first image must not wait for it.
    pub fn list(&self, dir: Arc<Path>, generation: u64) {
        let (tx, s) = (self.tx.clone(), self.shared.clone());
        std::thread::spawn(move || {
            let files = crate::files::list(&dir);
            let _ = tx.send(Msg::Listed { generation, files });
            wake(&s);
        });
    }
}

/// Free big buffers off the UI thread. Handing 100 MB back to the system takes milliseconds —
/// done in the key handler, the last reference to a full decode dying there made every flip a
/// frame late.
pub fn drop_later<T: Send + 'static>(x: T) {
    type Bin = Box<dyn Send>;
    static TX: std::sync::OnceLock<Mutex<Sender<Bin>>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<Bin>();
        std::thread::spawn(move || while rx.recv().is_ok() {});
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(Box::new(x));
}

fn wake(s: &Shared) {
    let hwnd = s.hwnd.load(Ordering::Acquire);
    // Before the window exists nobody is woken: the main thread reads the channel once it
    // has a window anyway.
    if hwnd != 0 {
        // SAFETY: posting to a window handle is safe even if the window is gone.
        unsafe { PostMessageW(hwnd as _, WM_LOADED, 0, 0) };
    }
}

fn worker(s: Arc<Shared>, tx: Sender<Msg>) {
    let wic = Wic::new();
    if let Some(w) = &wic {
        w.warm();
    }
    loop {
        let key = {
            let mut q = s.queue.lock().unwrap();
            loop {
                if let Some(k) = q.pop_front() {
                    break k;
                }
                q = s.cv.wait(q).unwrap();
            }
        };
        if !s.wanted.lock().unwrap().contains(&key) {
            continue;
        }
        if key.1 == THUMB {
            crate::trace("      [thumb] taken by a worker");
        }
        s.running.lock().unwrap().insert(key.clone());
        let t = s.target.load(Ordering::Relaxed);
        let target = (key.1 == PREVIEW).then_some(((t >> 32) as u32, t as u32));
        let still = || s.wanted.lock().unwrap().contains(&key);
        let img = match &wic {
            Some(w) if key.1 == THUMB => w.thumbnail(&key.0),
            Some(w) => w.decode(&key.0, target, s.max_dim.load(Ordering::Relaxed), &still),
            None => Err("WIC is not available".into()),
        };
        crate::trace(&format!("worker: {} {} {}", key.0.file_name().unwrap_or_default().to_string_lossy(), ["thumb", "preview", "full"][key.1 as usize], if img.is_ok() { "done" } else { "failed" }));
        s.running.lock().unwrap().remove(&key);
        if matches!(&img, Err(e) if e == "cancelled") {
            continue;
        }
        let _ = tx.send(Msg::Decoded { path: key.0.clone(), kind: key.1, img: img.map(Arc::new) });
        wake(&s);
    }
}
