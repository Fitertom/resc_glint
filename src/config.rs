//! `glint.ini` next to the exe: `key = value`, `#` comments. Written with the defaults on the
//! first run, so the knobs are there to edit.

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
pub enum WindowMode {
    Normal,
    Maximized,
    Fullscreen,
}

pub struct Config {
    pub window: WindowMode,
    /// Wheel zooms (true) or flips images (false; Ctrl+wheel then zooms).
    pub wheel_zoom: bool,
    /// Past the last image comes the first.
    pub wrap: bool,
    /// Stretch images smaller than the window up to it on fit.
    pub upscale_small: bool,
    /// Keep zoom and pan when flipping, for comparing shots of one scene.
    pub keep_zoom: bool,
    pub background: [f32; 3],
    pub info: bool,
    pub prefetch_ahead: usize,
    pub prefetch_behind: usize,
    pub prefer_integrated: bool,
    /// Load implicit Vulkan layers (OBS, Steam overlay, RTSS...). Each one is a DLL loaded and
    /// initialised at start; off by default because that is where most start-up time goes.
    pub implicit_layers: bool,
    /// Stay in memory after the window is closed (hidden, working set trimmed): the next
    /// photo opens in the running process, with Vulkan already up.
    pub resident: bool,
    /// Give memory back while hidden (~2 MB), at the price of paging it in on the next open.
    pub trim_memory: bool,
    /// The setup card was answered (applied or put off); it does not come by itself again.
    pub setup_done: bool,
    /// Where the window was when it was last closed: left, top, right, bottom of its normal
    /// (not maximised) rectangle in screen pixels, and whether it was maximised.
    pub placement: Option<([i32; 4], bool)>,
}

const DEFAULT: &str = "\
# Glint settings. Delete this file to get the defaults back.

# normal | maximized | fullscreen
window = maximized
# zoom | navigate   (with navigate, Ctrl+wheel zooms)
wheel = zoom
# after the last image comes the first
wrap = true
# stretch small images up to the window on fit
upscale_small = false
# keep zoom/pan when switching images (compare series)
keep_zoom = false
# background, hex RRGGBB
background = 1b1b1b
# info bar at the bottom (I toggles)
info = true
# how many images to decode ahead of / behind the current one
prefetch_ahead = 4
prefetch_behind = 2
# discrete | integrated
gpu = discrete
# Vulkan implicit layers (OBS/Steam/RTSS overlays). false = faster start
implicit_layers = false
# stay in memory when closed (a few MB); the next photo opens instantly. --register adds autostart
resident = true
# while hidden: give memory back (~2 MB, opening pages it back in) or keep it (faster open)
trim_memory = true
";

impl Config {
    pub fn load() -> Config {
        let path = path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                let _ = std::fs::write(&path, DEFAULT);
                DEFAULT.to_string()
            }
        };
        let mut c = Config {
            window: WindowMode::Maximized,
            wheel_zoom: true,
            wrap: true,
            upscale_small: false,
            keep_zoom: false,
            background: [0.106, 0.106, 0.106],
            info: true,
            prefetch_ahead: 4,
            prefetch_behind: 2,
            prefer_integrated: false,
            implicit_layers: false,
            resident: true,
            trim_memory: true,
            setup_done: false,
            placement: None,
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let yes = matches!(v, "true" | "1" | "yes" | "on");
            match k {
                "window" => {
                    c.window = match v {
                        "normal" => WindowMode::Normal,
                        "fullscreen" => WindowMode::Fullscreen,
                        _ => WindowMode::Maximized,
                    }
                }
                "wheel" => c.wheel_zoom = v != "navigate",
                "wrap" => c.wrap = yes,
                "upscale_small" => c.upscale_small = yes,
                "keep_zoom" => c.keep_zoom = yes,
                "info" => c.info = yes,
                "background" => {
                    if let Ok(x) = u32::from_str_radix(v.trim_start_matches('#'), 16) {
                        c.background = [(x >> 16 & 255) as f32 / 255.0, (x >> 8 & 255) as f32 / 255.0, (x & 255) as f32 / 255.0];
                    }
                }
                "prefetch_ahead" => c.prefetch_ahead = v.parse().unwrap_or(4).min(32),
                "prefetch_behind" => c.prefetch_behind = v.parse().unwrap_or(2).min(32),
                "gpu" => c.prefer_integrated = v == "integrated",
                "implicit_layers" => c.implicit_layers = yes,
                "resident" => c.resident = yes,
                "trim_memory" => c.trim_memory = yes,
                "setup_done" => c.setup_done = yes,
                "placement" => {
                    let v: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if let [l, t, r, b, m] = v[..] {
                        c.placement = Some(([l, t, r, b], m != 0));
                    }
                }
                _ => {}
            }
        }
        c
    }

    /// Background as a GDI `COLORREF`, for the window class brush shown before the first frame.
    pub fn colorref(&self) -> u32 {
        let [r, g, b] = self.background.map(|x| (x * 255.0).round() as u32);
        r | g << 8 | b << 16
    }
}

/// Set one key in the file, keeping everything else as the user wrote it.
pub fn store(key: &str, value: &str) {
    store_at(&path(), key, value);
}

/// The same, in the settings file of another copy (the installed one).
pub fn store_at(path: &std::path::Path, key: &str, value: &str) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| DEFAULT.to_string());
    let mut found = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| match l.split_once('=') {
            Some((k, _)) if k.trim() == key => {
                found = true;
                format!("{key} = {value}")
            }
            _ => l.to_string(),
        })
        .collect();
    if !found {
        out.push(format!("{key} = {value}"));
    }
    let _ = std::fs::write(path, out.join("\n") + "\n");
}

fn path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_file_name("glint.ini")
}
