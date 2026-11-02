//! FluxBridge developer tasks.
//!
//! Currently a single task, `icon`, which regenerates `assets/icon.ico` from
//! `assets/icon.svg`. This crate is excluded from the root workspace so its
//! rendering dependencies never touch the normal `cargo build --workspace` or
//! the shipped desktop binary. Invoke it explicitly:
//!
//! ```sh
//! cargo run --manifest-path xtask/Cargo.toml -- icon
//! ```

use std::error::Error;
use std::path::{Path, PathBuf};

use resvg::tiny_skia;
use usvg::Transform;

/// The icon sizes embedded in the multi-resolution Windows .ico, in pixels.
/// 16/32/48 cover the taskbar, Explorer and shortcut sizes; 256 is the modern
/// high-DPI tile. All frames are square (the wider-than-tall glyph is centered
/// with transparent padding).
const ICON_SIZES: &[u32] = &[16, 32, 48, 256];

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    let result = match task.as_str() {
        "icon" => generate_icon(),
        other => {
            eprintln!("unknown task {other:?}; available tasks: icon");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("xtask {task} failed: {e}");
        std::process::exit(1);
    }
}

/// Repository root, derived from this crate's manifest directory (xtask/..).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf()
}

/// Rasterize `assets/icon.svg` into each size in [`ICON_SIZES`], centered on a
/// transparent square canvas, and write the assembled `assets/icon.ico`.
fn generate_icon() -> Result<(), Box<dyn Error>> {
    let root = repo_root();
    let svg_path = root.join("assets").join("icon.svg");
    let ico_path = root.join("assets").join("icon.ico");

    let svg_data = std::fs::read(&svg_path)?;
    let tree = usvg::Tree::from_data(&svg_data, &usvg::Options::default())?;

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for &size in ICON_SIZES {
        let rgba = render_square(&tree, size);
        let image = ico::IconImage::from_rgba_data(size, size, rgba.clone());
        icon_dir.add_entry(ico::IconDirEntry::encode(&image)?);

        // The 256 px frame is also written out as a standalone PNG. The Slint
        // GUI build embeds this PNG (include_bytes!) to set the live window
        // icon, which avoids depending on a runtime file path in the installed
        // app. See crates/desktop/src/ui.rs.
        if size == 256 {
            let png_path = root.join("assets").join("icon-256.png");
            write_png(&png_path, size, size, &rgba)?;
            println!("wrote {}", png_path.display());
        }
    }

    let file = std::fs::File::create(&ico_path)?;
    icon_dir.write(file)?;
    println!(
        "wrote {} ({} sizes: {:?})",
        ico_path.display(),
        ICON_SIZES.len(),
        ICON_SIZES
    );
    Ok(())
}

/// Encode a tightly-packed RGBA buffer as an 8-bit RGBA PNG.
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(())
}

/// Render the SVG tree into a `size` x `size` transparent RGBA buffer, scaling
/// the (wider-than-tall) artwork to fit and centering it so the square has even
/// transparent padding on the shorter axis.
fn render_square(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("non-zero pixmap size");

    let svg_size = tree.size();
    let (w, h) = (svg_size.width(), svg_size.height());
    // Uniform scale so the longer artwork axis fits the square exactly.
    let scale = (size as f32) / w.max(h);
    // Center: offset the shorter axis by half the leftover space.
    let tx = (size as f32 - w * scale) / 2.0;
    let ty = (size as f32 - h * scale) / 2.0;
    let transform = Transform::from_scale(scale, scale).post_translate(tx, ty);

    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap.take()
}
