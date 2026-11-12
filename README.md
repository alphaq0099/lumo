# lumo

A fast, lightweight image viewer for **Windows and Linux** with a single-window,
selection-bounded navigation model, comic-album support and standard editing.

Built with Rust + egui. One dependency-free binary (~7 MB release, gl renderer),
starts instantly, ~130 MB RSS including the GL/X11 stack.

```
cargo build --release
```

## Why "selection-bounded"?

Open five images from your file manager (multi-select → open, or drag the
selection onto the lumo window, or pass them on the command line) and you get
**one window** holding exactly those five files. `→`/`Space` steps through
*your selection* — nothing else in the folder is ever loaded. Opening a CBZ/CBR/
EPUB/PDF expands it into its pages and navigation walks the album; `PgUp`/`PgDn`
jump to the previous/next *file* in your selection.

## Formats

| Group | Formats |
| --- | --- |
| Built in | PNG/APNG, JPEG/JFIF, GIF, WebP, BMP/DIB, TIFF, TGA, ICO/CUR, PNM/PPM/PGM/PBM, QOI, DDS, EXR, HDR, farbfeld |
| Optional features | JPEG XL (`jxl`), AVIF, SVG (`svg`), PDF (`pdf`, via pdfium) |
| Albums | CBZ, ZIP, EPUB (images inside), CBR, RAR |
| Not included | HEIC/HEIF — needs a system `libheif`; see below |

EXIF orientation is applied automatically. Animated GIF/WebP show their first
frame (animation playback is on the roadmap).

## Keys

| Key | Action |
| --- | --- |
| `→` / `Space` | next page |
| `←` / `Backspace` | previous page |
| `PgUp` / `PgDn` | previous / next file in the selection |
| `Ctrl`+`→` / Ctrl+`←` | previous / next file |
| `Home` / `End` | first / last page of current file |
| Mouse wheel | zoom (anchored at cursor), drag to pan |
| `0` / `1` | fit to window / 100% |
| `+` / `-` (`=` / `-`) | zoom in / out |
| `Ctrl`+`O` | open files (appended to the current selection) |
| `Ctrl`+`R` / `Ctrl`+`L` | rotate right / left |
| `Ctrl`+`H` / `Ctrl`+`V` | flip horizontal / vertical |
| `Ctrl`+`X` | crop mode (drag a box, `Enter` to apply, `Esc` to cancel) |
| `B/W` button / `Ctrl`+`A` *panel* | grayscale, brightness/contrast/saturation panel, resize, sharpen |
| `Ctrl`+`Z` | undo (per page edit stack) |
| `Ctrl`+`S` | save — overwrites the original, keeps a one-time `.original.` backup |
| `Ctrl`+`Shift`+`S` | save as (PNG/JPEG/WebP/TIFF/BMP/TGA/QOI/…, JPEG q92 / auto defaults) |
| `Ctrl`+`C` | copy image to clipboard |
| `F11` / `F` | fullscreen |

The toolbar mirrors the common actions: prev/next, a view-mode button that
shows the current zoom (`fit`, `1:1`, or a percentage — click to switch
between fit and 100%), rotate left/right, flip horizontal/vertical, crop,
undo, open and save. When the zoomed image is larger than the window,
overlay scrollbars appear at the right and bottom edges and can be dragged
to pan.

Pages extracted from archives/PDFs cannot be overwritten in place; `Ctrl+S`
refuses and points at `Save As`.

## PDF (and EPUB) notes

PDF rendering uses the `pdfium` dynamic library (optional `pdf` feature, on by
default). Lumo looks for it at:

1. the path in `$LUMO_PDFIUM`,
2. `pdfium.dll` / `libpdfium.so` next to the executable (or in a `lib/` folder
   next to it),
3. the system library path.

Fetch a prebuilt copy:

- Linux: `scripts/get-pdfium.sh` (downloads to `./lib/libpdfium.so`)
- Windows: `scripts/get-pdfium.ps1` (downloads `pdfium.dll`)

EPUBs are treated like CBZ: lumo lists the image files inside the container in
natural order (cover, pages…). Text-only EPUBs show an empty album.

## HEIC/HEIF (roadmap)

Compile-time feature `heic` is reserved; HEIF containers are detected and fail
with a clear message. Full support arrives via optional `libheif` linkage — the
decode cache and preview pipeline are already in place.

## Windows Explorer integration

Add right-click **"Open with lumo"** that receives your whole multi-selection
(`MultiSelectModel="Player"`): double-click `windows/lumo-open.reg` and fix the
exe path inside it first. Drag-and-drop onto the window also works anywhere,
and works with mixed selections (images + albums + PDFs).

## Linux desktop integration

`linux/lumo.desktop` is included — put it (with the binary) in `~/.local/share`
and `update-desktop-database` it; `%U` passes every selected file.

## CLI

```
lumo img1.png img2.jpg comic.cbz scan.pdf     # one window, two files + two albums
```

## Layout

```
src/
  main.rs     CLI, eframe bootstrap (glow renderer)
  app.rs      the viewer: nav, zoom/pan, crop, panels, texture cache
  doc.rs      Document/PageSource model + expansion, format table
  archive.rs  CBZ/ZIP/EPUB listing, RAR read, natural sort
  decode.rs   background decode worker, EXIF orientation, svg/jxl/avif sniffing
  edit.rs     edit ops + apply (rotate/flip/crop/resize/brightness/contrast/sat/sharpen)
  save.rs     save-as & overwrite-with-backup
  pdf.rs      pdfium bindings (optional)
```

## Build for Windows (cross or native)

On a Windows machine with Rust + VS Build Tools:

```
cargo build --release
```

Produces `target\release\lumo.exe`. Or cross-compile from Linux with
[cargo-xwin](https://github.com/rust-cross/cargo-xwin):

```
cargo xwin build --release --target x86_64-pc-windows-msvc
```

## Status / roadmap

Working: everything above. Roadmap: image animations (APNG/GIF/WebP),
re-redo (redo stack), drag-out-to-move-file, single-instance forwarding,
extended editor (red-eye, text/watermark, batch convert), HEIC via libheif.
