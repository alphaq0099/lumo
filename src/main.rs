//! lumo — a fast, light single-window image viewer for Windows & Linux.
//!
//! Usage: lumo <file1> [file2 ...]
//! All files passed on the command line (or dropped onto the window) become
//! one navigable list in a single window. Albums (CBZ/CBR/ZIP/RAR/EPUB/PDF)
//! expand into their pages automatically.

// Don't spawn a console window in release builds on Windows (debug builds
// keep it so stderr logging is visible during development).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod archive;
mod decode;
mod doc;
mod edit;
mod icons;
mod pdf;
mod save;

use doc::Document;
use std::path::PathBuf;

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.is_empty())
        .collect();

    let paths: Vec<PathBuf> = args
        .into_iter()
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .collect();

    let docs: Vec<Document> = paths
        .iter()
        .filter(|p| p.is_file())
        .map(|p| doc::expand(p.as_path()))
        .take(2000)
        .collect();

    let app = app::Lumo::new(docs);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([420.0, 300.0])
            .with_title("lumo")
            .with_app_id("lumo"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "lumo",
        options,
        Box::new(move |_cc| Ok(Box::new(app))),
    )
}
