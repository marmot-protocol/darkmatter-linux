//! Starter-profile pictures for freshly generated accounts: each animal from
//! the random "[Adjective] [Animal]" name has an SVG face in
//! `assets/animals/`, rasterized at publish time over a vertical gradient
//! derived from the account's npub (same hash family as the in-app fallback
//! avatars), then uploaded to Blossom as a PNG. PNG — not the SVG itself —
//! because our own avatar pipeline (and most Nostr clients) decode profile
//! pictures with raster codecs only.

use anyhow::{anyhow, Context, Result};
use resvg::tiny_skia;
use resvg::usvg;

/// Output edge in pixels. 512 keeps the flat art crisp on any avatar size the
/// UI renders while staying a small upload (~10-30 KB of flat-color PNG).
const SIZE: u32 = 512;

/// The canonical animal list: `random_profile_name` picks from this table, so
/// every name that can be generated is guaranteed to have art.
pub const ANIMAL_SVGS: &[(&str, &str)] = &[
    ("Bear", include_str!("../assets/animals/bear.svg")),
    ("Fox", include_str!("../assets/animals/fox.svg")),
    ("Otter", include_str!("../assets/animals/otter.svg")),
    ("Lynx", include_str!("../assets/animals/lynx.svg")),
    ("Raven", include_str!("../assets/animals/raven.svg")),
    ("Owl", include_str!("../assets/animals/owl.svg")),
    ("Wolf", include_str!("../assets/animals/wolf.svg")),
    ("Hare", include_str!("../assets/animals/hare.svg")),
    ("Badger", include_str!("../assets/animals/badger.svg")),
    ("Marmot", include_str!("../assets/animals/marmot.svg")),
    ("Falcon", include_str!("../assets/animals/falcon.svg")),
    ("Heron", include_str!("../assets/animals/heron.svg")),
    ("Newt", include_str!("../assets/animals/newt.svg")),
    ("Mole", include_str!("../assets/animals/mole.svg")),
    ("Stoat", include_str!("../assets/animals/stoat.svg")),
    ("Walrus", include_str!("../assets/animals/walrus.svg")),
    ("Panda", include_str!("../assets/animals/panda.svg")),
    ("Koala", include_str!("../assets/animals/koala.svg")),
    ("Gecko", include_str!("../assets/animals/gecko.svg")),
    ("Ferret", include_str!("../assets/animals/ferret.svg")),
    ("Moose", include_str!("../assets/animals/moose.svg")),
    ("Bison", include_str!("../assets/animals/bison.svg")),
    ("Crane", include_str!("../assets/animals/crane.svg")),
    ("Finch", include_str!("../assets/animals/finch.svg")),
    ("Hedgehog", include_str!("../assets/animals/hedgehog.svg")),
    ("Kestrel", include_str!("../assets/animals/kestrel.svg")),
    ("Lemur", include_str!("../assets/animals/lemur.svg")),
    ("Mongoose", include_str!("../assets/animals/mongoose.svg")),
    ("Narwhal", include_str!("../assets/animals/narwhal.svg")),
    ("Ocelot", include_str!("../assets/animals/ocelot.svg")),
    ("Puffin", include_str!("../assets/animals/puffin.svg")),
    ("Quokka", include_str!("../assets/animals/quokka.svg")),
    ("Raccoon", include_str!("../assets/animals/raccoon.svg")),
    ("Tapir", include_str!("../assets/animals/tapir.svg")),
    ("Wombat", include_str!("../assets/animals/wombat.svg")),
    ("Yak", include_str!("../assets/animals/yak.svg")),
    ("Penguin", include_str!("../assets/animals/penguin.svg")),
    ("Toad", include_str!("../assets/animals/toad.svg")),
    ("Capybara", include_str!("../assets/animals/capybara.svg")),
    ("Pangolin", include_str!("../assets/animals/pangolin.svg")),
];

/// SVG source for an animal name (case-insensitive), e.g. the last word of a
/// generated display name.
pub fn svg_for(animal: &str) -> Option<&'static str> {
    ANIMAL_SVGS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(animal))
        .map(|(_, svg)| *svg)
}

/// Rasterize `svg` over a vertical `top`→`bottom` gradient and encode as PNG.
pub fn render_png(svg: &str, top: (u8, u8, u8), bottom: (u8, u8, u8)) -> Result<Vec<u8>> {
    let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE)
        .ok_or_else(|| anyhow!("allocate {SIZE}x{SIZE} pixmap"))?;

    // Background gradient.
    let shader = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(0.0, SIZE as f32),
        vec![
            tiny_skia::GradientStop::new(
                0.0,
                tiny_skia::Color::from_rgba8(top.0, top.1, top.2, 255),
            ),
            tiny_skia::GradientStop::new(
                1.0,
                tiny_skia::Color::from_rgba8(bottom.0, bottom.1, bottom.2, 255),
            ),
        ],
        tiny_skia::SpreadMode::Pad,
        tiny_skia::Transform::identity(),
    )
    .ok_or_else(|| anyhow!("build gradient shader"))?;
    let paint = tiny_skia::Paint {
        shader,
        ..Default::default()
    };
    let full = tiny_skia::Rect::from_xywh(0.0, 0.0, SIZE as f32, SIZE as f32)
        .ok_or_else(|| anyhow!("full-canvas rect"))?;
    pixmap.fill_rect(full, &paint, tiny_skia::Transform::identity(), None);

    // Animal art, scaled to fill the canvas.
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default())
        .map_err(|e| anyhow!("parse svg: {e}"))?;
    let scale = SIZE as f32 / tree.size().width().max(1.0);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // The gradient makes every pixel opaque, so the premultiplied buffer is
    // already straight RGBA — encode directly.
    let mut png = Vec::new();
    use image::ImageEncoder;
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixmap.data(), SIZE, SIZE, image::ExtendedColorType::Rgba8)
        .context("encode png")?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every animal must parse and rasterize. Writes 4x2 inspection sheets
    /// (256px cells) to `target/animal-sheet-N.png` for eyeballing the art.
    #[test]
    fn render_all_animals() {
        const CELL: u32 = 256;
        for (sheet_idx, chunk) in ANIMAL_SVGS.chunks(8).enumerate() {
            let mut sheet = image::RgbaImage::new(4 * CELL, 2 * CELL);
            for (i, (name, svg)) in chunk.iter().enumerate() {
                // Spread the test gradients around so the sheets show the art
                // on a variety of plausible backgrounds.
                let n = sheet_idx * 8 + i;
                let t = (140 + (n * 29) % 100) as u8;
                let png = render_png(svg, (t, 130, 220), (50, 38, 84))
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                let img = image::load_from_memory(&png)
                    .unwrap_or_else(|e| panic!("{name}: decode: {e}"))
                    .to_rgba8();
                let thumb = image::imageops::thumbnail(&img, CELL, CELL);
                let (x, y) = (((i as u32) % 4) * CELL, ((i as u32) / 4) * CELL);
                image::imageops::overlay(&mut sheet, &thumb, x as i64, y as i64);
            }
            sheet
                .save(format!("target/animal-sheet-{sheet_idx}.png"))
                .expect("save sheet");
        }
    }
}
