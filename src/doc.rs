//! Document model: what lumo opens, and how it expands into pages.
//!
//! A `Document` is one logical item — a single image, a comic archive
//! (CBZ/CBR/ZIP/RAR/EPUB), or a PDF. It expands into an ordered list of
//! `PageSource`s. Navigation lives inside a document's pages; the user's
//! selection sets the list boundaries — lumo never reads the folder around it.

use std::path::{Path, PathBuf};

/// Extensions we consider images (used for listing archive members and
/// deciding whether a stray file counts as a page).
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jfif", "png", "gif", "bmp", "webp", "tif", "tiff", "tga", "ico", "cur",
    "qoi", "dds", "ppm", "pgm", "pbm", "pnm", "avif", "jxl", "heic", "heif", "svg", "svgz",
    "exr", "hdr", "ff",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// A single image file.
    Image,
    /// CBZ / ZIP / EPUB / CBR / RAR: an ordered album of pages.
    Album,
    /// PDF: pages rendered on demand.
    Pdf,
}

/// Where one page's pixels come from.
#[derive(Debug, Clone)]
pub enum PageSource {
    File(PathBuf),
    /// A file inside a zip-based container (CBZ/ZIP/EPUB).
    ZipMember { path: PathBuf, name: String },
    /// A file inside a rar container (CBR/RAR).
    RarMember { path: PathBuf, name: String },
    /// Page `index` (0-based) of a PDF.
    PdfPage { path: PathBuf, index: i32 },
}

impl PageSource {
    pub fn describe(&self) -> String {
        match self {
            PageSource::File(p) => p.display().to_string(),
            PageSource::ZipMember { name, .. } | PageSource::RarMember { name, .. } => {
                name.clone()
            }
            PageSource::PdfPage { path, index } => format!("{} · page {}", path.display(), index + 1),
        }
    }
}

/// One opened item: an image, an album, or a PDF.
#[derive(Debug)]
pub struct Document {
    #[allow(dead_code)] // `path` reserved for rename/relate features; `kind` drives labels

    pub path: PathBuf,
    pub label: String,
    pub kind: DocKind,
    /// Expanded page list. If opening failed, this is empty and `error` is set.
    pub pages: Vec<PageSource>,
    pub error: Option<String>,
}

#[allow(dead_code)]
impl Document {
    pub fn page_label(&self, page: usize) -> String {
        match self.kind {
            DocKind::Image => self.label.clone(),
            DocKind::Album | DocKind::Pdf => {
                format!("{} of {}", page + 1, self.pages.len().max(1))
            }
        }
    }
}

pub fn is_image_ext(ext: &str) -> bool {
    IMAGE_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Turn one path from the selection into a Document with its page list.
pub fn expand(path: &Path) -> Document {
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());

    match ext_of(path).as_str() {
        "cbz" | "zip" | "epub" => match crate::archive::zip_image_names(path) {
            Ok(names) if !names.is_empty() => Document {
                path: path.into(),
                label: label.clone(),
                kind: DocKind::Album,
                pages: names
                    .into_iter()
                    .map(|name| PageSource::ZipMember { path: path.into(), name })
                    .collect(),
                error: None,
            },
            Ok(_) => Document {
                path: path.into(),
                label,
                kind: DocKind::Album,
                pages: vec![],
                error: Some("Archive contains no images.".into()),
            },
            Err(e) => Document {
                path: path.into(),
                label,
                kind: DocKind::Album,
                pages: vec![],
                error: Some(format!("Can't open archive: {e}")),
            },
        },
        "cbr" | "rar" => match crate::archive::rar_image_names(path) {
            Ok(names) if !names.is_empty() => Document {
                path: path.into(),
                label: label.clone(),
                kind: DocKind::Album,
                pages: names
                    .into_iter()
                    .map(|name| PageSource::RarMember { path: path.into(), name })
                    .collect(),
                error: None,
            },
            Ok(_) => Document {
                path: path.into(),
                label,
                kind: DocKind::Album,
                pages: vec![],
                error: Some("Archive contains no images.".into()),
            },
            Err(e) => Document {
                path: path.into(),
                label,
                kind: DocKind::Album,
                pages: vec![],
                error: Some(format!("Can't open archive: {e}")),
            },
        },
        "pdf" => Document {
            path: path.into(),
            label: label.clone(),
            kind: DocKind::Pdf,
            #[cfg(feature = "pdf")]
            pages: match crate::pdf::pdf_page_count(path) {
                Ok(count) => (0..count)
                    .map(|index| PageSource::PdfPage { path: path.into(), index })
                    .collect(),
                Err(e) => {
                    return Document {
                        path: path.into(),
                        label,
                        kind: DocKind::Pdf,
                        pages: vec![],
                        error: Some(format!("{e}")),
                    };
                }
            },
            #[cfg(not(feature = "pdf"))]
            pages: vec![],
            error: if cfg!(not(feature = "pdf")) {
                Some("This build has no PDF support (rebuild with --features pdf).".into())
            } else {
                None
            },
        },
        _ => Document {
            path: path.into(),
            label,
            kind: DocKind::Image,
            pages: vec![PageSource::File(path.into())],
            error: None,
        },
    }
}
