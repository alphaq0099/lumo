//! Saving: overwrite-with-backup and Save As in any supported format.

use anyhow::Context;
use image::{DynamicImage, ImageFormat};
use std::path::Path;

/// Re-encode `image` and write it to `path`, honoring the extension.
/// `jpeg_q` is the JPEG quality (1-100).
pub fn save_as(image: &DynamicImage, path: &Path, jpeg_q: u8) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let mut buf: Vec<u8> = Vec::new();
    match ext.as_str() {
        "jpg" | "jpeg" | "jfif" => {
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, jpeg_q.clamp(1, 100));
            enc.encode_image(image).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        "png" | "apng" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "webp" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::WebP)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "tiff" | "tif" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Tiff)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "bmp" | "dib" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Bmp)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "tga" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Tga)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "ico" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Ico)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "qoi" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Qoi)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "pnm" | "ppm" | "pgm" | "pbm" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Pnm)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "dds" => image
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Dds)
            .map_err(|e| anyhow::anyhow!("{e}"))?,
        "" => {
            // No extension: save as PNG.
            image
                .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        _ => anyhow::bail!("Saving to .{ext} isn't supported."),
    }

    std::fs::write(path, &buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Overwrite the original file, keeping a one-time backup of the untouched
/// bytes next to it (`name.jpg.original.jpg`).
///
/// Note: page sources inside archives can't be overwritten in place —
/// lumo refuses and directs the user at Save As.
pub fn overwrite(
    image: &DynamicImage,
    source_path: &Path,
    has_edits: bool,
    jpeg_q: u8,
) -> anyhow::Result<()> {
    if !has_edits {
        anyhow::bail!("Nothing to save: this page has no edits.");
    }
    if !std::fs::metadata(source_path).map(|m| m.is_file()).unwrap_or(false) {
        anyhow::bail!(
            "This page lives inside an archive/PDF. Use Save As (Ctrl+Shift+S) to export it."
        );
    }

    if let Some(backup) = backup_path(source_path) {
        if !backup.exists() {
            std::fs::copy(source_path, &backup).ok();
        }
    }

    save_as(image, source_path, jpeg_q)
}

fn backup_path(p: &Path) -> Option<std::path::PathBuf> {
    let ext = p.extension()?;
    let stem = p.file_stem()?;
    let mut b = p.to_path_buf();
    b.set_file_name(format!(
        "{}.original.{}",
        stem.to_string_lossy(),
        ext.to_string_lossy()
    ));
    Some(b)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adjust {
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
}

impl Default for Adjust {
    fn default() -> Self {
        Self { brightness: 0, contrast: 0, saturation: 0 }
    }
}

impl Adjust {
    pub fn is_neutral(&self) -> bool {
        self.brightness == 0 && self.contrast == 0 && self.saturation == 0
    }
}
