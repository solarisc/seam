//! Renders Seam's icon from `assets/icon/*.svg` into the files each platform
//! uses. Run from the repository root after editing either SVG:
//!
//! ```sh
//! cargo run --release --manifest-path tools/icons/Cargo.toml
//! ```
use std::{error::Error, fs::File, io::BufWriter, path::Path};

/// Sizes at or below this use the simplified drawing, which stays sharp.
const SMALL: u32 = 32;

fn main() -> Result<(), Box<dyn Error>> {
    let full = load("assets/icon/seam.svg")?;
    let small = load("assets/icon/seam-small.svg")?;
    let render = |size: u32| -> Result<resvg::tiny_skia::Pixmap, Box<dyn Error>> {
        let tree = if size <= SMALL { &small } else { &full };
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).ok_or("invalid size")?;
        let scale = size as f32 / tree.size().width();
        resvg::render(
            tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap)
    };

    render(256)?.save_png("assets/icon-256.png")?;

    let mut ico = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, unpremultiplied(&render(size)?));
        // Classic BMP entries below 256 pixels: some Windows code reads only those.
        ico.add_entry(if size < 256 {
            ico::IconDirEntry::encode_as_bmp(&image)?
        } else {
            ico::IconDirEntry::encode_as_png(&image)?
        });
    }
    ico.write(BufWriter::new(File::create("assets/seam.ico")?))?;

    let mut icns = icns::IconFamily::new();
    for size in [16, 32, 64, 128, 256, 512, 1024] {
        let image = icns::Image::from_data(
            icns::PixelFormat::RGBA,
            size,
            size,
            unpremultiplied(&render(size)?),
        )?;
        icns.add_icon(&image)?;
    }
    icns.write(BufWriter::new(File::create("assets/seam.icns")?))?;
    println!("Wrote assets/icon-256.png, assets/seam.ico and assets/seam.icns");
    Ok(())
}

fn load(path: &str) -> Result<resvg::usvg::Tree, Box<dyn Error>> {
    let data = std::fs::read(Path::new(path))?;
    Ok(resvg::usvg::Tree::from_data(
        &data,
        &resvg::usvg::Options::default(),
    )?)
}

/// tiny-skia keeps premultiplied alpha; icon formats expect straight alpha.
fn unpremultiplied(pixmap: &resvg::tiny_skia::Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect()
}
