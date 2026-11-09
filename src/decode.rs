//! Background decoding: worker thread + dispatch by page source.

use crate::doc::PageSource;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::mpsc;

/// Identifies one page across all docs: (doc index, page index).
pub type PageKey = (usize, usize);

pub enum Request {
    Fetch { key: PageKey, source: PageSource },
}

pub type Reply = (PageKey, anyhow::Result<DecodedImage>);

pub struct DecodedImage {
    pub image: DynamicImage,
}

pub fn spawn() -> (mpsc::Sender<Request>, mpsc::Receiver<Reply>) {
    let (tx, rx) = mpsc::channel::<Request>();
    let (data_tx, data_rx) = mpsc::channel::<Reply>();

    std::thread::Builder::new()
        .name("lumo-decode".into())
        .spawn(move || {
            let mut zips = crate::archive::ZipPool::new();
            while let Ok(req) = rx.recv() {
                match req {
                    Request::Fetch { key, source } => {
                        let result = decode_page(&mut zips, &source);
                        // Channel may be gone at shutdown; ignore.
                        let _ = data_tx.send((key, result));
                    }
                }
            }
        })
        .expect("spawn decode worker");

    (tx, data_rx)
}

fn decode_page(zips: &mut crate::archive::ZipPool, source: &PageSource) -> anyhow::Result<DecodedImage> {
    let bytes: Vec<u8> = match source {
        PageSource::File(path) => std::fs::read(path)?,
        PageSource::ZipMember { path, name } => zips.read_member(path, name)?,
        PageSource::RarMember { path, name } => crate::archive::read_rar_member(path, name)?,
        PageSource::PdfPage { path, index } => {
            return crate::pdf::pdf_render_page(path, *index)
                .map(|image| DecodedImage { image });
        }
    };
    decode_bytes(&bytes, &guess_format_for(source)).map(|image| DecodedImage { image })
}

fn guess_format_for(source: &PageSource) -> Option<ImageFormat> {
    match source {
        PageSource::File(p) => format_from_ext(&p.extension()?.to_string_lossy()),
        PageSource::ZipMember { name, .. } | PageSource::RarMember { name, .. } => {
            let ext = PathBuf::from(name)
                .extension()?
                .to_string_lossy()
                .to_string();
            format_from_ext(&ext)
        }
        PageSource::PdfPage { .. } => None,
    }
}

pub fn format_from_ext(ext: &str) -> Option<ImageFormat> {
    let e = ext.to_ascii_lowercase();
    Some(match e.as_str() {
        "jpg" | "jpeg" | "jfif" => ImageFormat::Jpeg,
        "png" | "apng" => ImageFormat::Png,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::WebP,
        "tif" | "tiff" => ImageFormat::Tiff,
        "bmp" | "dib" => ImageFormat::Bmp,
        "ico" | "cur" => ImageFormat::Ico,
        "tga" => ImageFormat::Tga,
        "pnm" | "ppm" | "pgm" | "pbm" => ImageFormat::Pnm,
        "qoi" => ImageFormat::Qoi,
        "dds" => ImageFormat::Dds,
        "exr" => ImageFormat::OpenExr,
        "hdr" => ImageFormat::Hdr,
        // handled by special decoders below
        "jxl" | "avif" | "svg" | "svgz" | "heic" | "heif" => return None,
        _ => return None,
    })
}

/// Decode raw bytes of an image (single frame: first GIF/WebP frame, EXIF
/// orientation applied).svg / JXL / AVIF go through their own decoders.
pub fn decode_bytes(bytes: &[u8], forced: &Option<ImageFormat>) -> anyhow::Result<DynamicImage> {
    // Sniff known magic signatures first.
    if starts_with_svg(bytes) {
        return decode_svg(bytes);
    }
    if is_jxl(bytes) {
        return decode_jxl(bytes);
    }
    if is_heif(bytes) {
        anyhow::bail!(
            "HEIC/HEIF needs a system libheif; this build doesn't include it."
        );
    }

    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    if let Some(f) = forced {
        reader.set_format(*f);
    }

    // Decode with EXIF orientation applied.
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn starts_with_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head);
    s.trim_start().starts_with("<?xml")
        || s.trim_start().starts_with("<svg")
        || (s.contains("<svg") && s.contains("</svg>"))
}

fn is_jxl(bytes: &[u8]) -> bool {
    // naked JXL FF 0A, or JPEG-XL container 00 00 00 0C 4A 58 4C
    bytes.starts_with(&[0xFF, 0x0A])
        || bytes.starts_with(&[0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' '])
}

fn is_heif(bytes: &[u8]) -> bool {
    bytes.len() > 12
        && &bytes[4..8] == b"ftyp"
        && (bytes[8..12] == [b'h', b'e', b'i', b'c']
            || bytes[8..12] == [b'm', b'i', b'f', b'1']
            || bytes[8..12] == [b'h', b'e', b'i', b'f']
            || bytes[8..12] == [b'm', b's', b'f', b'1'])
}

// ---------------------------------------------------------------------------

#[cfg(feature = "svg")]
fn decode_svg(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &opt)
        .map_err(|e| anyhow::anyhow!("SVG parse error: {e}"))?;
    let size = tree.size();
    let w = size.width().round().max(1.0) as u32;
    let h = size.height().round().max(1.0) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| anyhow::anyhow!("SVG canvas too large"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let data = pixmap.data(); // RGBA8 premultiplied (resvg outputs straight? premultiplied)
    // resvg premultiplies; unpremultiply to keep egui/Image happy
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        if a == 0 || a == 255 {
            out.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
        } else {
            out.push(((255 * r + a / 2) / a) as u8);
            out.push(((255 * g + a / 2) / a) as u8);
            out.push(((255 * b + a / 2) / a) as u8);
            out.push(a as u8);
        }
    }
    Ok(DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(w, h, out).ok_or_else(|| anyhow::anyhow!("SVG decode"))?,
    ))
}

#[cfg(not(feature = "svg"))]
fn decode_svg(_bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    Err(anyhow::anyhow!("This build has no SVG support (feature `svg`)."))
}

#[cfg(feature = "jxl")]
fn decode_jxl(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    let image = jxl_oxide::JxlImage::builder()
        .read(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("JXL error: {e}"))?;
    let render = image
        .render_frame(0)
        .map_err(|e| anyhow::anyhow!("JXL frame error: {e}"))?;
    let buf = render.image_all_channels();
    let (w, h, ch) = (buf.width(), buf.height(), buf.channels());
    if ch < 3 {
        anyhow::bail!("JXL: unexpected channel count {ch}");
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    let raw = buf.buf();
    for n in 0..w * h {
        let r = raw[n * ch];
        let g = raw[n * ch + 1];
        let b = raw[n * ch + 2];
        let a = if ch > 3 { raw[n * ch + 3] } else { 1.0 };
        rgba.extend_from_slice(&[to8(r), to8(g), to8(b), to8(a)]);
    }
    Ok(DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(w as u32, h as u32, rgba)
            .ok_or_else(|| anyhow::anyhow!("JXL decode"))?,
    ))
}

#[cfg(feature = "jxl")]
fn to8(f: f32) -> u8 {
    (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(not(feature = "jxl"))]
fn decode_jxl(_bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    Err(anyhow::anyhow!("This build has no JPEG XL support (feature `jxl`)."))
}
