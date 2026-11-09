//! PDF rendering through pdfium (optional `pdf` feature).
//!
//! pdfium is a small dynamic library; on Windows it ships as `pdfium.dll`,
//! on Linux as `libpdfium.so`. Lumo looks for it at the `LUMO_PDFIUM`
//! environment variable, next to the executable, `./lib`, or the system
//! library path. See scripts/get-pdfium.* to download a build.


#[cfg(feature = "pdf")]
mod inner {
    use anyhow::Context as _;
    use pdfium_render::prelude::*;

    pub fn pdfium() -> anyhow::Result<Pdfium> {
        let bindings: Option<std::path::PathBuf> = match std::env::var_os("LUMO_PDFIUM")
            .map(std::path::PathBuf::from)
        {
            Some(p) => Some(p),
            None => {
                let exe = std::env::current_exe()?.parent().map(|p| p.to_path_buf());
                exe.and_then(|dir| {
                    for name in pdfium_lib_names() {
                        for sub in ["", "lib"] {
                            let cand = dir.join(sub).join(&name);
                            if cand.is_file() {
                                return Some(cand);
                            }
                        }
                    }
                    None
                })
            }
        };

        let bound = match bindings {
            Some(p) => Pdfium::bind_to_library(p)
                .or_else(|_| Pdfium::bind_to_system_library()),
            None => Pdfium::bind_to_system_library(),
        }
        .context("PDF support needs the pdfium library. Run scripts/get-pdfium.sh (Linux) \
                  or scripts/get-pdfium.ps1 (Windows), or install it system-wide.")?;

        Ok(Pdfium::new(bound))
    }

    #[cfg(target_os = "windows")]
    fn pdfium_lib_names() -> Vec<String> {
        vec!["pdfium.dll".into()]
    }
    #[cfg(target_os = "linux")]
    fn pdfium_lib_names() -> Vec<String> {
        vec!["libpdfium.so".into()]
    }
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    fn pdfium_lib_names() -> Vec<String> {
        vec!["libpdfium.dylib".into()]
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    fn pdfium_lib_names() -> Vec<String> {
        Vec::new()
    }

    pub fn pdf_page_count(path: &std::path::Path) -> anyhow::Result<i32> {
        let pdfium = pdfium()?;
        let doc = pdfium.load_pdf_from_file(path, None)?;
        Ok(doc.pages().len())
    }

    pub fn pdf_render_page(path: &std::path::Path, index: i32) -> anyhow::Result<image::DynamicImage> {
        let pdfium = pdfium()?;
        let doc = pdfium.load_pdf_from_file(path, None)?;
        let page = doc.pages().get(index)?;
        let config = PdfRenderConfig::new().scale_page_by_factor(2.5);
        Ok(page.render_with_config(&config)?.as_image()?)
    }
}

#[cfg(feature = "pdf")]
pub use inner::{pdf_page_count, pdf_render_page};

#[cfg(not(feature = "pdf"))]
pub fn pdf_page_count(_path: &std::path::Path) -> anyhow::Result<i32> {
    Err(anyhow::anyhow!("This build has no PDF support (feature `pdf`)."))
}

#[cfg(not(feature = "pdf"))]
pub fn pdf_render_page(_path: &std::path::Path, _index: i32) -> anyhow::Result<DynamicImage> {
    Err(anyhow::anyhow!("This build has no PDF support (feature `pdf`)."))
}
