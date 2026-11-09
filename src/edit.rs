//! Edit operations and their application to decoded images.

use image::{imageops::FilterType, DynamicImage};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditOp {
    Rotate90,
    Rotate270,
    FlipH,
    FlipV,
    /// Pixel rect in the image's coordinate space at the time the crop was made.
    Crop { x: i32, y: i32, w: i32, h: i32 },
    Resize { w: u32, h: u32 },
    /// -100..=100
    Brightness(i32),
    /// -100..=100 (0 = unchanged)
    Contrast(i32),
    /// -100..=100 (0 = unchanged, -100 = grayscale, +100 = double saturation)
    Saturation(i32),
    Grayscale,
    Sharpen,
}

pub fn apply_ops(img: &DynamicImage, ops: &[EditOp]) -> DynamicImage {
    let mut out = img.clone();
    for op in ops {
        out = apply_one(&out, *op);
    }
    out
}

fn apply_one(img: &DynamicImage, op: EditOp) -> DynamicImage {
    match op {
        EditOp::Rotate90 => img.rotate90(),
        EditOp::Rotate270 => img.rotate270(),
        EditOp::FlipH => img.fliph(),
        EditOp::FlipV => img.flipv(),
        EditOp::Crop { x, y, w, h } => {
            let (iw, ih) = (img.width() as i32, img.height() as i32);
            let x = x.clamp(0, iw.saturating_sub(1));
            let y = y.clamp(0, ih.saturating_sub(1));
            let w = w.clamp(1, iw - x);
            let h = h.clamp(1, ih - y);
            img.crop_imm(x as u32, y as u32, w.max(1) as u32, h.max(1) as u32)
        }
        EditOp::Resize { w, h } => img.resize_exact(w.clamp(1, u16::MAX as u32), h.clamp(1, u16::MAX as u32), FilterType::Triangle),
        EditOp::Brightness(v) => img.brighten(v),
        EditOp::Contrast(v) => contrast(img, v),
        EditOp::Saturation(v) => saturation(img, v as f32 / 100.0),
        EditOp::Grayscale => img.grayscale(),
        EditOp::Sharpen => {
            const K: [f32; 9] = [0.0, -1.0, 0.0, -1.0, 5.0, -1.0, 0.0, -1.0, 0.0];
            img.filter3x3(&K)
        }
    }
}

/// Standard contrast formula, scaled so the op value (-100..100) maps to a
/// sensible range (factor 0.34..3.9).
fn contrast(img: &DynamicImage, v: i32) -> DynamicImage {
    let c = v as f32; // -100..=100
    let factor = (c / 100.0 + 1.0).max(0.0); // 0..2 around neutral

    let clamp = |f: f32| f.round().clamp(0.0, 255.0) as u8;
    let mut dst = img.to_rgba8();
    for px in dst.pixels_mut() {
        let [r, g, b, a] = px.0;
        let (r, g, b) = (r as f32, g as f32, b as f32);
        let f = |v: f32| clamp(128.0 + (v - 128.0) * factor);
        px.0 = [f(r), f(g), f(b), a];
    }
    DynamicImage::ImageRgba8(dst)
}

fn saturation(img: &DynamicImage, factor: f32) -> DynamicImage {
    let clamp = |f: f32| f.round().clamp(0.0, 255.0) as u8;
    let mut dst = img.to_rgba8();
    for px in dst.pixels_mut() {
        let [r, g, b, a] = px.0;
        let (r, g, b) = (r as f32, g as f32, b as f32);
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        let f = |c: f32| clamp(gray + (c - gray) * (1.0 + factor));
        px.0 = [f(r), f(g), f(b), a];
    }
    DynamicImage::ImageRgba8(dst)
}

pub fn signature(ops: &[EditOp]) -> String {
    ops.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>().join(";")
}
