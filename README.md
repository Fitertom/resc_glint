# Glint

Glint is an image viewer for Windows 10 and 11, built to open photos as fast as possible. It is written in Rust directly on top of Win32 and Vulkan. It has no UI framework and no runtime dependencies, and ships as a single 1.1 MB executable.

## Key figures

Measured on an AMD Ryzen 7 5800X, NVIDIA GeForce RTX 3060, 48 GB RAM, Windows 11 LTSC 24H2 (build 26100). Test files were a 6000 x 4000 JPEG (2.3 MB) and a 1200 x 800 PNG with alpha. Times come from the built-in trace (`GLINT_TRACE=1`).

| Scenario | Time |
|---|---|
| Open a file in the resident process: request received to window on screen with the image | 3-5 ms |
| Open through the shell (`ShellExecuteEx`, warm shell): call to window on screen | 12-16 ms |
| Same, through a launcher process instead of `DelegateExecute`, for comparison | 22-28 ms |
| Thumbnail placeholder, read directly from the Explorer thumbnail cache | ~1 ms |
| Same placeholder through the Shell API (`IShellItemImageFactory`), for comparison | ~8 ms |
| Switch to the next, already prefetched image | 1.2-1.9 ms |
| Cold start without the resident process: window on screen | ~25 ms |
| Cold start without the resident process: first photo on screen | 220-250 ms |

| Resource | Value |
|---|---|
| Executable size (release, statically linked CRT) | 1.12 MB |
| Resident process, idle working set | 2-4 MB |
| Resident process, idle CPU | 0 |

Most of a cold start is `vkCreateInstance` in the NVIDIA driver (~125 ms), which is outside the application's control. This is why Glint keeps a resident process by default (see below).

The resident open time is measured from the moment the running process receives the request. The shell figures cover the whole path from a `ShellExecuteEx` call, which is what a double click in Explorer runs: association lookup and COM activation in Windows, then the open in Glint.

## Features

- Formats: JPEG, PNG, GIF, BMP, TIFF, ICO, DDS and JPEG XR out of the box. WebP, HEIC/HEIF, AVIF and JPEG XL work where the matching Windows codec extensions are installed. All decoding goes through Windows Imaging Component (WIC).
- Instant placeholder: while the image decodes, Glint shows the thumbnail Explorer has already cached. If there is none, it shows the EXIF thumbnail.
- Scaled JPEG decoding: the preview is decoded at 1/2, 1/4 or 1/8 resolution in the codec itself, matched to the screen. The full image follows in the background.
- Prefetching: neighbouring images are decoded ahead and kept on the GPU, so flipping costs one frame.
- Zoom and pan, trilinear filtering when zoomed out and sharp bilinear when zoomed in. Rotation. EXIF orientation is applied without touching the pixel data.
- Frameless window with its own caption, a bottom toolbar and fullscreen mode. Window size and position are restored per monitor.
- Reference mode: pin the window on top of others, make it see-through (10-100 %), or open the current image in a separate window sized to it. Tooltips on every button.
- Tray icon: a left click opens the image from the clipboard, a right click opens the menu.
- One-click setup: install for the current user, start with Windows, and register as the default image viewer.

## Requirements

- Windows 10 or 11, x64.
- A GPU with Vulkan 1.3 support.
- Building from source: Rust 1.85 or newer (edition 2024), MSVC toolchain. The Vulkan SDK is optional: if `glslc` is not found, the build uses the precompiled shaders in `shaders/`.

## Building

```
cargo build --release
```

The release profile uses fat LTO, a single codegen unit, `panic = "abort"`, stripped symbols and a statically linked C runtime (`.cargo/config.toml`). The resulting `target/release/glint.exe` has no dependencies beyond Windows itself.

## Installation

Run `glint.exe` once. A setup card offers three options, applied with one button:

- **Install:** copies the program to `%LOCALAPPDATA%\Programs\Glint`, adds a Start menu shortcut and registers an uninstaller under Apps & Features.
- **Start with Windows:** launches the resident process at logon.
- **Open images by default:** registers the file associations and sets Glint as the default handler for supported extensions.

The card can be opened again from the gear button in the caption or from the tray menu. Everything is installed per user, so no administrator rights are needed.

Command line:

| Argument | Action |
|---|---|
| `glint.exe <file>` | Open a file (handed to the resident process if one is running) |
| `--background` | Start the resident process hidden |
| `--register` / `--unregister` | Set or remove autostart and file associations |
| `--uninstall` | Remove the installation |
| `--quit` | Stop the resident process |

## Controls

| Input | Action |
|---|---|
| Right, Page Down, Space / Left, Page Up, Backspace | Next / previous image |
| Home / End | First / last image in the folder |
| Mouse wheel, Up / Down, + / - | Zoom in / out at the cursor |
| 0 | Fit to window |
| 1 | Actual size (100%) |
| Double click, F, F11, Enter | Toggle fullscreen |
| R / Shift+R or L | Rotate clockwise / counter-clockwise |
| I, Tab | Toggle the info line |
| B | Cycle the background: configured colour, black, light grey, checkerboard |
| Delete | Move to the Recycle Bin |
| O | Open a file |
| T | Keep the window on top (pin) |
| [ / ] | Opacity down / up; the caption's opacity button also takes a click, a drag or the wheel |
| N | Open the current image in a separate window |
| Esc | Leave fullscreen, or close |
| Q | Close |

## Configuration

Settings live in `glint.ini` next to the executable. It is created with defaults and comments on first run.

| Key | Default | Meaning |
|---|---|---|
| `window` | `maximized` | `normal`, `maximized` or `fullscreen` |
| `wheel` | `zoom` | `zoom`, or `navigate` (Ctrl+wheel then zooms) |
| `wrap` | `true` | After the last image comes the first |
| `upscale_small` | `false` | Stretch small images up to the window on fit |
| `keep_zoom` | `false` | Keep zoom and pan when switching images |
| `background` | `1b1b1b` | Background colour, hex RRGGBB |
| `info` | `true` | Info line in the caption |
| `prefetch_ahead` / `prefetch_behind` | `4` / `2` | Images decoded ahead of and behind the current one |
| `gpu` | `discrete` | `discrete` or `integrated` |
| `implicit_layers` | `false` | Load implicit Vulkan layers (overlays); off for faster start |
| `resident` | `true` | Stay in memory after the window is closed |
| `trim_memory` | `true` | Release the working set while hidden |

## Architecture

### Stack

- **Rust**, edition 2024. The crates in the release binary are `ash` (Vulkan bindings), `windows-sys` (Win32 bindings) and `windows-link`.
- **Win32** for the window, input, tray, clipboard, registry and shell integration. COM interfaces that `windows-sys` does not cover are called through their vtables directly.
- **Vulkan 1.3** for rendering, using dynamic rendering and synchronization2, a single pipeline and one frame in flight.
- **WIC** for decoding, so the system codecs cover every format Windows supports and no codec code ships in the binary.
- **GDI** to draw text and icons into a bitmap, which is then uploaded as a texture.
- **Build time:** `resvg` renders the SVG logo into the icon resource and the in-app logo. `glslc` compiles the shaders.

### Modules

| Module | Responsibility |
|---|---|
| `main.rs` | Argument handling, hand-over to a running instance, start-up sequence, tracing |
| `win.rs` | Window, message loop, frameless window handling, cloaking, tray icon, fullscreen |
| `app.rs` | Application state: navigation, zoom and pan, caching policy, events, frame composition |
| `gpu.rs` | Vulkan device, swapchain, textures with mip chains, staging, drawing |
| `loader.rs` | Worker pool for decoding, prioritised and cancellable jobs |
| `wic.rs` | Decoding through WIC: scaled previews, full decodes in bands, thumbnails |
| `thumbcache.rs` | Direct reader for the Explorer thumbnail cache |
| `ui.rs` | Caption, toolbar and setup card, drawn with GDI and antialiased shapes |
| `files.rs` | Folder listing in Explorer order, file dialog, Recycle Bin |
| `config.rs` | `glint.ini` reading and writing |
| `install.rs`, `assoc.rs` | Per-user installation, autostart and file associations |
| `com.rs`, `com_server.rs` | Minimal COM helpers and a local COM server for shell verbs |
| `clipboard.rs` | Images from the clipboard (files, PNG, DIB) |

### Opening a file

1. Explorer resolves the "open" verb. Glint registers it with `DelegateExecute`, so no process is started: COM calls `IExecuteCommand::Execute` on the resident instance, through its message loop. If no instance is running, COM starts one with `-Embedding`. Launches from the command line use the older route: the new process hands the path to the resident instance (`WM_COPYDATA`) and exits.
2. The resident process queues a thumbnail job and the preview decode for the file, and the thumbnail jobs for its neighbours. The folder is listed on a worker thread.
3. The thumbnail comes from the Explorer cache in about 1 ms. It is uploaded and drawn together with the caption.
4. The window, which stayed shown but cloaked through DWM, is uncloaked. This takes about 0.1 ms, against 3-18 ms for `ShowWindow` on a hidden window.
5. The screen-sized preview replaces the placeholder when it is ready. The full-resolution image follows if the user zooms in.

### Resident process

When the window is closed, the process releases decoded images and textures, trims its working set and cloaks the window. The Vulkan instance, device, swapchain and pipeline stay alive, and they are what make the next open fast. The start of the staging buffer is touched again after trimming, and small GPU memory blocks go back to a pool instead of being freed. As a result, the first frame of the next open causes no page faults and no driver allocations. The idle footprint is 2-4 MB of working set and no CPU.

### Thumbnail cache reader

Getting a thumbnail through the Shell API costs about 5 ms to parse the path into a shell item, plus about 2 ms for the cache query. Glint reads the cache files in `%LOCALAPPDATA%\Microsoft\Windows\Explorer` directly instead:

- **Key:** the cache key (ThumbnailCacheId) is a 64-bit hash of the volume GUID, the NTFS file reference, the file extension and the modification time. The time is a DOS timestamp followed by its sub-2-second rounding remainder. Glint computes it from one `GetFileInformationByHandle` call. The algorithm was reconstructed from `CFSFolder::_GetThumbnailCacheId` in `windows.storage.dll` (Windows 11), building on the published analysis of the Windows 7 version.
- **Index:** `thumbcache_idx.db` is an open-addressing hash table with linear probing. Each entry gives the offset of the thumbnail in each size-specific database.
- **Data:** the entry in `thumbcache_256.db`, or in the larger databases, is checked against the key and decoded with WIC.

The format is undocumented. Any mismatch (a different database version, a non-NTFS volume, a cloud placeholder, or a missing entry) sends Glint back to the Shell API.

## Limitations

- Windows only, and a Vulkan 1.3 capable GPU is required.
- The thumbnail cache reader targets the Windows 11 cache format (version 0x20). Other versions fall back to the slower Shell path.
- Animated GIF and multi-page TIFF show the first frame only.

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## License

MIT, see [LICENSE](LICENSE).
