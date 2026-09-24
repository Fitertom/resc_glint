//! Build steps: the app's icon and caption logo from `assets/logo.svg`, and the shaders.
//!
//! **The icon is written into the executable as a Windows resource** (this part is the
//! editor's `build.rs`). The window's own icon is not enough for the taskbar: the taskbar
//! button, Alt-Tab and Explorer take the icon of the EXECUTABLE. No resource compiler is
//! needed: a compiled resource file (`.res`) is a plain binary format, and the MSVC linker
//! takes one as an input like an object file.
//!
//! **The SVG is rasterised here, not at run time**: `resvg` is a build dependency only, and
//! the exe gets finished pixels — every icon size drawn from the curves at that size, sharper
//! than shrinking one bitmap.

use std::path::PathBuf;

const SOURCE: &str = "assets/logo.svg";
/// Sizes put into the icon: the taskbar picks 32 or 48 by the screen's scale, Explorer 256,
/// the title bar 16.
const SIZES: [usize; 4] = [256, 48, 32, 16];
/// The caption's logo, premultiplied BGRA; shrunk at run time to the caption's size.
const LOGO_SIDE: usize = 64;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const LANG_EN_US: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    shaders();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let tree = load_svg();
    let logo: Vec<u8> = render(&tree, LOGO_SIDE, true).chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    std::fs::write(out_dir.join("logo.bgra"), logo).expect("logo.bgra was not written");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !(windows && msvc) {
        return;
    }

    let mut res = Vec::new();
    // A .res file opens with an empty entry: that is how the linker knows it is 32-bit.
    entry(&mut res, 0, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    for (k, &side) in SIZES.iter().enumerate() {
        let dib = dib(&render(&tree, side, false), side);
        let id = k as u16 + 1;
        entry(&mut res, RT_ICON, id, 0x1010, LANG_EN_US, &dib);
        let byte = if side >= 256 { 0 } else { side as u8 };
        group.extend_from_slice(&[byte, byte, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    // THE FIRST GROUP ICON IS THE EXECUTABLE'S ICON — the lowest number is the one Explorer
    // and the taskbar take.
    entry(&mut res, RT_GROUP_ICON, 1, 0x1030, LANG_EN_US, &group);

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icon.res");
    std::fs::write(&out, res).expect("icon.res was not written");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// One resource: a header naming its type and number by ordinal, then the data, padded to
/// four bytes.
fn entry(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, lang: u16, data: &[u8]) {
    let head = [
        &(data.len() as u32).to_le_bytes()[..],
        &32u32.to_le_bytes(),
        &0xFFFFu16.to_le_bytes(),
        &kind.to_le_bytes(),
        &0xFFFFu16.to_le_bytes(),
        &id.to_le_bytes(),
        &0u32.to_le_bytes(),
        &flags.to_le_bytes(),
        &lang.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    out.extend_from_slice(&head);
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// The SVG without its square dark ground: the mark is the ring alone, on transparency, as the
/// editor's icon always was. The editor's leftovers (`opacity="NaN"`) are dropped too.
fn load_svg() -> resvg::usvg::Tree {
    let svg = std::fs::read_to_string(SOURCE).expect("the logo source is missing");
    let svg = svg.replace(r#"fill="url(#bg)""#, r#"fill="none""#).replace(r#"opacity="NaN""#, "").replace(r#"opacity="undefined""#, "");
    resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("logo.svg does not parse")
}

/// Where the mark itself lies on the SVG's canvas: without its dark ground the ring fills only
/// about 70 % of it, and drawn by the canvas the taskbar icon came out small beside its
/// neighbours. Found by drawing once and reading the alpha.
fn bounds(tree: &resvg::usvg::Tree) -> (f32, f32, f32) {
    const PROBE: u32 = 1024;
    let mut pm = resvg::tiny_skia::Pixmap::new(PROBE, PROBE).unwrap();
    let k = PROBE as f32 / tree.size().width().max(tree.size().height());
    resvg::render(tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pm.as_mut());
    let (mut x0, mut y0, mut x1, mut y1) = (PROBE, PROBE, 0, 0);
    for (i, p) in pm.pixels().iter().enumerate() {
        if p.alpha() > 8 {
            let (x, y) = (i as u32 % PROBE, i as u32 / PROBE);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
        }
    }
    // A square around the mark's centre, in the SVG's own units.
    let side = (x1 - x0).max(y1 - y0) as f32 / k;
    let (cx, cy) = ((x0 + x1) as f32 / 2.0 / k, (y0 + y1) as f32 / 2.0 / k);
    (cx - side / 2.0, cy - side / 2.0, side)
}

/// The mark drawn at `side`×`side`, filling it edge to edge, RGBA: premultiplied, or
/// straight as an icon wants it.
fn render(tree: &resvg::usvg::Tree, side: usize, premultiplied: bool) -> Vec<u8> {
    let mut pm = resvg::tiny_skia::Pixmap::new(side as u32, side as u32).unwrap();
    let (x, y, s) = bounds(tree);
    let k = side as f32 / s;
    resvg::render(tree, resvg::tiny_skia::Transform::from_row(k, 0.0, 0.0, k, -x * k, -y * k), &mut pm.as_mut());
    if premultiplied {
        return pm.data().to_vec();
    }
    pm.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }).collect()
}

/// An icon image as Windows keeps it: a bitmap header of twice the height (colour and mask),
/// the pixels bottom-up in BGRA, then a mask of zeros — the alpha says what is transparent.
fn dib(rgba: &[u8], side: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for v in [40u32, side as u32, (side * 2) as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for _ in 0..6 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    for y in (0..side).rev() {
        for x in 0..side {
            let p = (y * side + x) * 4;
            out.extend_from_slice(&[rgba[p + 2], rgba[p + 1], rgba[p], rgba[p + 3]]);
        }
    }
    let mask_row = side.div_ceil(32) * 4;
    out.extend(std::iter::repeat_n(0u8, mask_row * side));
    out
}

/// Shaders to SPIR-V with `glslc` from the Vulkan SDK. The compiled `.spv` files are kept in
/// `shaders/`, so a machine without the SDK still builds from them.
fn shaders() {
    use std::path::Path;
    for name in ["image.vert", "image.frag"] {
        let src = format!("shaders/{name}");
        let out = format!("shaders/{name}.spv");
        println!("cargo:rerun-if-changed={src}");
        let glslc = std::env::var("VULKAN_SDK").map(|s| Path::new(&s).join("Bin").join("glslc.exe"));
        match glslc {
            Ok(g) if g.exists() => {
                let st = std::process::Command::new(g)
                    .args(["-O", "--target-env=vulkan1.3", &src, "-o", &out])
                    .status()
                    .expect("glslc did not start");
                assert!(st.success(), "glslc failed on {src}");
            }
            _ => assert!(Path::new(&out).exists(), "no glslc (VULKAN_SDK) and no prebuilt {out}"),
        }
    }
}
