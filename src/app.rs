//! The egui application: single window, selection-bounded navigation,
//! zoom/pan, crop/rotate/flip/adjust editing, saving.

use crate::decode::{self, PageKey};
use crate::doc::{self, Document};
use crate::edit::{self, EditOp};
use crate::icons;
use crate::save::Adjust;
use egui::{
    Align2, Area, CentralPanel, Color32, ColorImage, Frame, Key, Panel, Pos2, Rect, Vec2,
};
use std::path::Path;
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc};
use std::time::Instant;

type Tex = egui::TextureHandle;

/// In-flight wheel-zoom animation: scale/pan blend from the old value to the
/// current target (`self.zoom` / `self.pan`).
#[derive(Clone, Copy, Debug)]
struct ZoomAnim {
    from_scale: f32,
    from_pan: Vec2,
    t: f32,
}

const ZOOM_ANIM_SPEED: f32 = 16.0; // ~0.19 s to settle

#[derive(Clone, Copy, Debug, PartialEq)]
enum Zoom {
    Fit,
    Scale(f32),
}

struct Entry {
    original: Option<Arc<image::DynamicImage>>,
    failed: Option<String>,
    #[allow(dead_code)] // rebuilt on demand in prepare(); cached for speed
    edited: Option<(String, Arc<image::DynamicImage>)>,
    texture: Option<Tex>,
    tex_sig: Option<String>,
    /// Decoded pixel dimensions, for memory accounting.
    dims: Option<(u32, u32)>,
    /// Where the pixels came from, so a dropped `original` can be re-decoded.
    source: Option<doc::PageSource>,
}

impl Entry {
    fn empty() -> Self {
        Self {
            original: None,
            failed: None,
            edited: None,
            texture: None,
            tex_sig: None,
            dims: None,
            source: None,
        }
    }

    /// Rough resident cost: the decoded pixels (if still held) plus the GPU
    /// texture copy (if uploaded).
    fn approx_bytes(&self) -> usize {
        let Some((w, h)) = self.dims else {
            return 0;
        };
        let px = (w as usize) * (h as usize) * 4;
        let mut b = 0;
        if self.original.is_some() {
            b += px;
        }
        if self.texture.is_some() {
            b += px;
        }
        b + 64
    }
}

/// Soft ceiling for the decoded-page cache. Pages are cheap to re-decode from
/// local files, so evict rather than grow without bound.

pub struct Lumo {
    docs: Vec<Document>,
    cur_doc: usize,
    cur_page: usize,

    worker_tx: mpsc::Sender<decode::Request>,
    worker_rx: mpsc::Receiver<decode::Reply>,
    pending: HashSet<PageKey>,
    cache: HashMap<PageKey, Entry>,
    cache_order: Vec<PageKey>,
    cache_bytes: usize,

    edits: HashMap<PageKey, Vec<EditOp>>,
    adjust: Adjust, // live-preview slider values

    zoom: Zoom,
    pan: Vec2,
    zoom_anim: Option<ZoomAnim>,
    wheel_zoom_pct: f32, // how much one wheel notch zooms, in %
    /// Drop decoded pages as soon as you leave them (see ⚙ menu).
    low_memory: bool,
    last_avail: Rect,
    fullscreen: bool,
    /// Fullscreen chrome auto-hide: show bars until this instant after the
    /// last mouse move (None while not fullscreen).
    chrome_until: Option<std::time::Instant>,
    last_pointer: Option<Pos2>,
    /// Animated bar visibility 1 = shown, 0 = hidden (fullscreen only).
    chrome_anim: f32,
    /// While set and in the future, keep repainting every frame to flush
    /// stale/torn frames after a window resize (e.g. fullscreen toggles).
    repaint_until: Option<std::time::Instant>,
    /// View size of the previous frame, to detect viewport resizes.
    last_view_size: Vec2,
    /// Setting: seconds the bars stay up after the last mouse move.
    chrome_hide_secs: f32,
    /// Setting: hide the window while switching fullscreen (masks the resize
    /// garbage on Windows). Default on for Windows builds.
    fs_mask: bool,
    /// Setting: double-click on the image toggles fullscreen (instead of
    /// fit ↔ 100%).
    dbl_fs: bool,
    // --- reading behavior ---
    /// Wheel turns pages instead of zooming (Ctrl+wheel still zooms).
    wheel_pages: bool,
    /// Keep zoom/pan when moving between pages.
    remember_zoom: bool,
    /// 0 = fit whole image, 1 = fit width, 2 = fit height.
    fit_mode: i32,
    /// Autoscroll speed in view-heights per second (hold ↓/↑).
    autoscroll_speed: f32,
    // --- slideshow ---
    sliding: bool,
    slide_next: Option<Instant>,
    /// Slideshow interval seconds.
    slide_secs: f32,
    /// Slideshow wraps to the first page at the end.
    slide_loop: bool,
    /// Wheel-line accumulator for the wheel-pages mode.
    wheel_page_acc: f32,
    // --- look ---
    /// Light theme instead of dark.
    theme_light: bool,
    /// View background color (0xRRGGBB).
    view_bg: u32,
    /// Smooth (linear) texture filtering vs pixelated (nearest).
    smooth_scale: bool,
    /// Hide the mouse cursor after this many idle seconds in fullscreen (0 = off).
    hide_cursor_secs: f32,
    cursor_until: Option<Instant>,
    // --- startup / window ---
    /// Restore the last window position and size.
    remember_win: bool,
    /// Open in fullscreen at launch.
    start_fs: bool,
    startup_done: bool,
    /// Reopen the previously opened files when launched without arguments.
    reopen_last: bool,
    /// Window rect to persist (windowed mode only).
    win_rect: Option<Rect>,
    /// Close-time save has already happened.
    exit_saved: bool,
    // --- files ---
    /// JPEG save quality 1-100.
    jpeg_q: f32,
    /// 0 = selection order, 1 = name A→Z, 2 = name Z→A, 3 = newest, 4 = oldest.
    sort_mode: i32,
    /// Decode cache budget in MB.
    cache_mb: f32,
    /// Natural panel heights, measured in windowed mode, used to animate.
    top_full_h: f32,
    status_full_h: f32,

    panel_open: bool,
    crop_mode: bool,
    crop_start: Option<Pos2>, // drag start in image coords
    crop_rect: Option<Rect>,  // finalized, image coords

    toast: Option<(String, Instant)>,
    flash: Option<(String, Instant)>,
    /// Window has been fitted to the monitor at least once.
    mon_clamped: bool,
}

enum Prepared {
    /// Nothing open: show the app-name splash.
    Empty,
    Loading,
    Failed(String),
    Ready {
        texture: Tex,
        #[allow(dead_code)] // future: overlays while the image is under adjustment
        edited: bool,
    },
}

impl Lumo {
    /// Read the remembered window rect from the config for main.rs to apply
    /// when CREATING the window (applied there, not via commands, because the
    /// viewport builder re-asserts its own size every frame otherwise).
    pub fn win_rect_from_conf() -> Option<(Pos2, Vec2)> {
        if Lumo::load_setting("remember_win", 1.0) < 0.5 {
            return None;
        }
        let x = Lumo::load_setting("win_x", 0.0);
        let y = Lumo::load_setting("win_y", 0.0);
        let w = Lumo::load_setting("win_w", 0.0);
        let h = Lumo::load_setting("win_h", 0.0);
        if w > 100.0 && h > 100.0 {
            Some((Pos2::new(x, y), Vec2::new(w, h)))
        } else {
            None
        }
    }

    pub fn new(docs: Vec<Document>) -> Self {
        let (worker_tx, worker_rx) = decode::spawn();
        // With no files on the command line and the setting on, restore the
        // previous session's files.
        let docs = if docs.is_empty() && Self::load_setting("reopen_last", 1.0) >= 0.5 {
            Self::load_last_session()
        } else {
            docs
        };
        // Window geometry is applied in main.rs before the window is created
        // (win_rect_from_conf); no restore commands needed here.
        let win_rect = None;
        Self {
            docs,
            cur_doc: 0,
            cur_page: 0,
            worker_tx,
            worker_rx,
            pending: HashSet::new(),
            cache: HashMap::new(),
            cache_order: Vec::new(),
            cache_bytes: 0,
            edits: HashMap::new(),
            adjust: Adjust::default(),
            zoom: Zoom::Fit,
            pan: Vec2::ZERO,
            zoom_anim: None,
            wheel_zoom_pct: Self::load_setting("wheel_zoom_pct", 13.0)
                .clamp(1.0, 80.0),
            low_memory: Self::load_setting("low_memory", 0.0) >= 0.5,
            last_avail: Rect::from_min_size(Pos2::ZERO, Vec2::splat(600.0)),
            fullscreen: false,
            chrome_until: None,
            last_pointer: None,
            chrome_anim: 1.0,
            repaint_until: None,
            last_view_size: Vec2::ZERO,
            chrome_hide_secs: Self::load_setting("chrome_hide_secs", 2.5).clamp(0.0, 10.0),
            fs_mask: Self::load_setting("fs_mask", if cfg!(windows) { 1.0 } else { 0.0 }) >= 0.5,
            dbl_fs: Self::load_setting("dbl_fs", 1.0) >= 0.5,
            wheel_pages: Self::load_setting("wheel_pages", 0.0) >= 0.5,
            remember_zoom: Self::load_setting("remember_zoom", 0.0) >= 0.5,
            fit_mode: Self::load_setting("fit_mode", 0.0).round() as i32,
            autoscroll_speed: Self::load_setting("autoscroll_speed", 0.7),
            sliding: false,
            slide_next: None,
            slide_secs: Self::load_setting("slide_secs", 4.0).clamp(0.5, 120.0),
            slide_loop: Self::load_setting("slide_loop", 0.0) >= 0.5,
            wheel_page_acc: 0.0,
            theme_light: Self::load_setting("theme_light", 0.0) >= 0.5,
            view_bg: Self::load_setting("view_bg", 0.0).round().clamp(0.0, 16_777_215.0) as u32,
            smooth_scale: Self::load_setting("smooth_scale", 1.0) >= 0.5,
            hide_cursor_secs: Self::load_setting("hide_cursor_secs", 0.0).clamp(0.0, 10.0),
            cursor_until: None,
            remember_win: Self::load_setting("remember_win", 1.0) >= 0.5,
            start_fs: Self::load_setting("start_fs", 0.0) >= 0.5,
            startup_done: false,
            reopen_last: Self::load_setting("reopen_last", 1.0) >= 0.5,
            win_rect,
            exit_saved: false,
            jpeg_q: Self::load_setting("jpeg_q", 92.0).clamp(1.0, 100.0),
            sort_mode: Self::load_setting("sort_mode", 0.0).round() as i32,
            cache_mb: Self::load_setting("cache_mb", 128.0).clamp(16.0, 2048.0),
            top_full_h: 88.0,
            status_full_h: 30.0,
            panel_open: false,
            crop_mode: false,
            crop_start: None,
            crop_rect: None,
            toast: None,
            flash: None,
            mon_clamped: false,
        }
    }

    // ------------------------------------------------------- navigation

    fn doc(&self) -> Option<&Document> {
        self.docs.get(self.cur_doc)
    }

    fn key(&self) -> PageKey {
        (self.cur_doc, self.cur_page)
    }

    fn pages(&self) -> usize {
        self.doc().map(|d| d.pages.len()).unwrap_or(0)
    }

    /// Cleanup when the shown page changes (keeps zoom when the setting says so).
    fn page_changed(&mut self) {
        if self.remember_zoom {
            self.crop_mode = false;
            self.crop_start = None;
            self.crop_rect = None;
            self.adjust = Adjust::default();
        } else {
            self.reset_view();
        }
        // Manual navigation restarts the slideshow timer.
        if self.sliding {
            self.slide_next =
                Some(Instant::now() + std::time::Duration::from_secs_f32(self.slide_secs.max(0.5)));
        }
    }

    fn goto(&mut self, doc: usize, page: usize) {
        self.cur_doc = doc.min(self.docs.len().saturating_sub(1));
        let pmax = self.docs.get(self.cur_doc).map(|d| d.pages.len()).unwrap_or(1);
        self.cur_page = if pmax == 0 { 0 } else { page.min(pmax - 1) };
        self.page_changed();
    }

    fn reset_view(&mut self) {
        self.zoom = Zoom::Fit;
        self.pan = Vec2::ZERO;
        self.zoom_anim = None;
        self.crop_mode = false;
        self.crop_start = None;
        self.crop_rect = None;
        self.adjust = Adjust::default();
    }

    /// Switch to 100% with the image centered in the viewport (the pan is
    /// then within the clamp range either way: centered when it fits,
    /// mid-range when it overflows).
    fn enter_zoom100(&mut self, ctx: &egui::Context) {
        self.zoom = Zoom::Scale(1.0);
        self.zoom_anim = None;
        let avail = self.last_avail;
        if let Some(px) = self.best_texture_size(ctx) {
            self.pan = (avail.size() - Vec2::new(px.x, px.y)) / 2.0;
        } else {
            self.pan = Vec2::ZERO;
        }
    }

    // ------------------------------------------------------- smooth zoom

    /// Advance the wheel-zoom animation one frame and returns nothing;
    /// the displayed values are read through `zoom_display`.
    fn zoom_tick(&mut self, ctx: &egui::Context) {
        if let Some(a) = self.zoom_anim.as_mut() {
            let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.05);
            a.t += dt * ZOOM_ANIM_SPEED;
            if a.t >= 1.0 {
                self.zoom_anim = None;
            }
        }
    }

    /// Displayed (scale, pan), blending the animation if one is running.
    fn zoom_display(&self, fit: f32) -> (f32, Vec2) {
        let (ts, tp) = match self.zoom {
            Zoom::Fit => (fit, Vec2::ZERO),
            Zoom::Scale(s) => (s, self.pan),
        };
        let Some(a) = self.zoom_anim else {
            return (ts, tp);
        };
        // ease-out cubic for a natural settle
        let t = 1.0 - (1.0 - a.t).powi(3);
        (a.from_scale + (ts - a.from_scale) * t, a.from_pan + (tp - a.from_pan) * t)
    }

    /// Start a new animation from wherever we are displayed right now.
    fn zoom_anim_start(&mut self, fit: f32) {
        let (s, p) = self.zoom_display(fit);
        self.zoom_anim = Some(ZoomAnim { from_scale: s, from_pan: p, t: 0.0 });
    }

    // ------------------------------------------------------- settings

    fn config_path() -> Option<std::path::PathBuf> {
        if let Some(v) = std::env::var_os("LUMO_CONFIG") {
            if !v.is_empty() {
                return Some(std::path::PathBuf::from(v));
            }
        }
        if cfg!(windows) {
            let appdata = std::env::var_os("APPDATA")?;
            return Some(
                std::path::PathBuf::from(appdata).join("lumo").join("lumo.conf"),
            );
        }
        let dir = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
            _ => match std::env::var_os("HOME") {
                Some(h) => std::path::PathBuf::from(h).join(".config"),
                None => return None,
            },
        };
        Some(std::path::PathBuf::from(dir).join("lumo").join("lumo.conf"))
    }

    fn load_setting(key: &str, default: f32) -> f32 {
        let Some(path) = Self::config_path() else {
            return default;
        };
        match std::fs::read_to_string(&path) {
            Ok(txt) => {
                for line in txt.lines() {
                    if let Some(v) = line.strip_prefix(key) {
                        let v = v.trim().trim_start_matches('=').trim();
                        if let Ok(p) = v.parse::<f32>() {
                            return p;
                        }
                    }
                }
                default
            }
            Err(_) => default,
        }
    }

    fn save_settings(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let txt = format!(
            "wheel_zoom_pct = {:.1}\nlow_memory = {}\nchrome_hide_secs = {:.1}\nfs_mask = {}\ndbl_fs = {}\n\
             wheel_pages = {}\nremember_zoom = {}\nfit_mode = {}\nautoscroll_speed = {:.2}\n\
             slide_secs = {:.1}\nslide_loop = {}\ntheme_light = {}\nview_bg = {}\nsmooth_scale = {}\n\
             hide_cursor_secs = {:.1}\nremember_win = {}\nstart_fs = {}\nreopen_last = {}\n\
             win_x = {:.0}\nwin_y = {:.0}\nwin_w = {:.0}\nwin_h = {:.0}\n\
             jpeg_q = {:.0}\nsort_mode = {}\ncache_mb = {:.0}\n",
            self.wheel_zoom_pct.round(),
            if self.low_memory { 1 } else { 0 },
            self.chrome_hide_secs.clamp(0.0, 10.0),
            if self.fs_mask { 1 } else { 0 },
            if self.dbl_fs { 1 } else { 0 },
            if self.wheel_pages { 1 } else { 0 },
            if self.remember_zoom { 1 } else { 0 },
            self.fit_mode.clamp(0, 2),
            self.autoscroll_speed.clamp(0.05, 5.0),
            self.slide_secs.clamp(0.5, 120.0),
            if self.slide_loop { 1 } else { 0 },
            if self.theme_light { 1 } else { 0 },
            self.view_bg,
            if self.smooth_scale { 1 } else { 0 },
            self.hide_cursor_secs.clamp(0.0, 10.0),
            if self.remember_win { 1 } else { 0 },
            if self.start_fs { 1 } else { 0 },
            if self.reopen_last { 1 } else { 0 },
            self.win_rect.map(|r| r.min.x).unwrap_or(0.0),
            self.win_rect.map(|r| r.min.y).unwrap_or(0.0),
            self.win_rect.map(|r| r.width()).unwrap_or(0.0),
            self.win_rect.map(|r| r.height()).unwrap_or(0.0),
            self.jpeg_q.clamp(1.0, 100.0).round(),
            self.sort_mode.clamp(0, 4),
            self.cache_mb.clamp(16.0, 2048.0).round(),
        );
        let _ = std::fs::write(&path, txt);
    }

    /// Save the currently open files for the next session (one path per line).
    fn save_last_session(&self) {
        let Some(cfg) = Self::config_path() else { return };
        let Some(dir) = cfg.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let mut txt = String::new();
        let mut seen = std::collections::HashSet::new();
        for d in &self.docs {
            let p = d.path.to_string_lossy().to_string();
            if !p.is_empty() && seen.insert(p.clone()) {
                txt.push_str(&p);
                txt.push('\n');
            }
        }
        let _ = std::fs::write(dir.join("last.txt"), txt);
    }

    fn load_last_session() -> Vec<Document> {
        let Some(cfg) = Self::config_path() else { return Vec::new() };
        let Some(dir) = cfg.parent() else { return Vec::new() };
        let Ok(txt) = std::fs::read_to_string(dir.join("last.txt")) else {
            return Vec::new();
        };
        txt.lines()
            .filter_map(|l| {
                let p = Path::new(l);
                (p.is_file()).then(|| p.to_path_buf())
            })
            .take(2000)
            .map(|p| doc::expand(&p))
            .collect()
    }

    fn next_page(&mut self) {
        if self.cur_page + 1 < self.pages() {
            self.cur_page += 1;
            self.page_changed();
        } else if self.cur_doc + 1 < self.docs.len() {
            self.goto(self.cur_doc + 1, 0);
        } else if !self.docs.is_empty() {
            self.goto(0, 0);
        }
    }

    fn prev_page(&mut self) {
        if self.cur_page > 0 {
            self.cur_page -= 1;
            self.page_changed();
        } else if !self.docs.is_empty() {
            let d = (self.cur_doc + self.docs.len() - 1) % self.docs.len();
            let n = self.docs.get(d).map(|x| x.pages.len()).unwrap_or(1);
            self.goto(d, n.saturating_sub(1));
        }
    }

    fn next_doc(&mut self) {
        if self.docs.is_empty() {
            return;
        }
        self.goto((self.cur_doc + 1) % self.docs.len(), 0);
    }

    fn prev_doc(&mut self) {
        if self.docs.is_empty() {
            return;
        }
        self.goto((self.cur_doc + self.docs.len() - 1) % self.docs.len(), 0);
    }

    // ------------------------------------------------------- opening

    pub fn open_paths(&mut self, paths: Vec<std::path::PathBuf>) {
        let mut paths = paths;
        if self.sort_mode != 0 {
            paths.sort_by(|a, b| {
                let ord = match self.sort_mode {
                    1 | 2 => nat_cmp(
                        &a.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                        &b.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                    ),
                    3 | 4 => a
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .cmp(&b.metadata().and_then(|m| m.modified()).ok()),
                    _ => std::cmp::Ordering::Equal,
                };
                if self.sort_mode == 2 || self.sort_mode == 4 {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        let newdocs: Vec<Document> = paths
            .iter()
            .filter(|p| p.is_file())
            .map(|p| doc::expand(p))
            .collect();
        if newdocs.is_empty() {
            return;
        }
        let jump = self.docs.len();
        self.docs.extend(newdocs);
        self.goto(jump, 0);
    }

    fn open_dialog(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Open images, albums or PDFs")
            .add_filter("All supported", crate::archive::ALL_EXTS)
            .add_filter("Comics", &["cbz", "cbr", "zip", "rar", "epub", "pdf"])
            .pick_files()
        {
            self.open_paths(paths);
        }
    }

    fn incoming_files(&mut self, ctx: &egui::Context) {
        let files = ctx.input(|i| i.raw.dropped_files.clone());
        let paths: Vec<_> = files
            .into_iter()
            .filter_map(|f| f.path().to_path_buf().into())
            .collect();
        if !paths.is_empty() {
            self.open_paths(paths);
        }
    }

    // ------------------------------------------------------- decoding

    fn request(&mut self, key: PageKey, source: &doc::PageSource) {
        if self.pending.insert(key) {
            let _ = self.worker_tx.send(decode::Request::Fetch {
                key,
                source: source.clone(),
            });
        }
    }

    fn ensure_current(&mut self) {
        if self.cache.contains_key(&self.key()) || self.pending.contains(&self.key()) {
            return;
        }
        if let Some(src) = self.doc().and_then(|d| d.pages.get(self.cur_page)).cloned() {
            self.request(self.key(), &src);
        }
    }

    fn prefetch(&mut self) {
        let cur = self.cur_doc;
        let curp = self.cur_page;
        let mut wants: Vec<(PageKey, doc::PageSource)> = Vec::new();

        if let Some(d) = self.docs.get(cur) {
            // In low-memory mode only the immediate neighbours are kept, so
            // there is no point decoding further ahead.
            let offsets: &[usize] = if self.low_memory {
                &[curp.wrapping_sub(1), curp + 1]
            } else {
                &[curp + 1, curp + 2, curp.wrapping_sub(1)]
            };
            for dp in offsets {
                let dp = *dp;
                if dp < d.pages.len() {
                    if let Some(src) = d.pages.get(dp) {
                        wants.push(((cur, dp), src.clone()));
                    }
                }
            }
            if curp == 0 {
                if let Some(nd) = self.docs.get(cur + 1) {
                    if let Some(first) = nd.pages.first() {
                        wants.push(((cur + 1, 0), first.clone()));
                    }
                }
            }
        }

        for (k, src) in wants {
            if !self.cache.contains_key(&k) {
                self.request(k, &src);
            }
        }
    }

    fn poll_worker(&mut self) {
        while let Ok((key, res)) = self.worker_rx.try_recv() {
            self.pending.remove(&key);
            let source = self
                .docs
                .get(key.0)
                .and_then(|d| d.pages.get(key.1))
                .cloned();
            let entry = match res {
                Ok(decode::DecodedImage { image }) => {
                    let dims = (image.width(), image.height());
                    Entry {
                        original: Some(Arc::new(image)),
                        dims: Some(dims),
                        source,
                        ..Entry::empty()
                    }
                }
                Err(e) => Entry {
                    failed: Some(format!("{e}")),
                    source,
                    ..Entry::empty()
                },
            };
            if self.cache.insert(key, entry).is_none() {
                self.cache_order.push(key);
            }
            self.evict();
        }
    }

    fn evict(&mut self) {
        self.cache_bytes = self.cache.values().map(|e| e.approx_bytes()).sum();
        let cap = (self.cache_mb.clamp(16.0, 2048.0) * 1024.0 * 1024.0) as usize;
        let cur = self.key();
        while self.cache_bytes > cap && self.cache_order.len() > 3 {
            // Evict the entry furthest from the current page (LRU-ish), but
            // never the page on screen.
            let Some(idx) = self
                .cache_order
                .iter()
                .enumerate()
                .filter(|(_, k)| **k != cur)
                .max_by_key(|(_, k)| {
                    let (d, p) = (k.0 as i64, k.1 as i64);
                    let (cd, cp) = (cur.0 as i64, cur.1 as i64);
                    -((d - cd).abs() + (p - cp).abs())
                })
                .map(|(i, _)| i)
            else {
                break;
            };
            let k = self.cache_order.remove(idx);
            if let Some(e) = self.cache.remove(&k) {
                self.cache_bytes = self.cache_bytes.saturating_sub(e.approx_bytes());
            }
        }
    }

    /// Drop the full-resolution `original` of pages we're not looking at and
    /// can't edit right now. The GPU texture stays, so flipping back is still
    /// instant; the pixels are re-decoded only if an edit is requested.
    /// Is `k` close enough to the current page to keep decoded in low-memory
    /// mode? Current page ±1, plus the page you'd land on in a neighbouring
    /// document (its first page going forward, its last page going back).
    fn in_low_memory_window(&self, k: PageKey) -> bool {
        let cur = self.key();
        if k == cur {
            return true;
        }
        if k.0 == cur.0 {
            return (k.1 as i64 - cur.1 as i64).abs() <= 1;
        }
        if k.0 + 1 == cur.0 {
            return self
                .docs
                .get(k.0)
                .is_some_and(|d| d.pages.len().saturating_sub(1) == k.1);
        }
        if k.0 == cur.0 + 1 {
            return k.1 == 0;
        }
        false
    }

    fn trim_cache(&mut self) {
        let cur = self.key();
        let keys: Vec<PageKey> = self.cache.keys().copied().collect();
        let mut removed: Vec<PageKey> = Vec::new();
        for k in keys {
            // The page on screen, and anything with unsaved edits, always stay.
            if k == cur || !self.effective_ops(k).is_empty() {
                continue;
            }
            if self.low_memory && !self.in_low_memory_window(k) {
                removed.push(k);
                continue;
            }
            let drop = self
                .cache
                .get(&k)
                .is_some_and(|e| e.original.is_some() && e.texture.is_some());
            if drop {
                if let Some(e) = self.cache.get_mut(&k) {
                    e.original = None;
                }
            }
        }
        for k in removed {
            self.cache.remove(&k);
            self.cache_order.retain(|x| *x != k);
        }
        self.cache_bytes = self.cache.values().map(|e| e.approx_bytes()).sum();
    }

    /// Keep the window inside the monitor (and out from under the taskbar).
    /// Runs until the window fits; the user can still resize afterwards.
    fn clamp_to_monitor(&mut self, ctx: &egui::Context) {
        if self.mon_clamped {
            return;
        }
        let vp = ctx.input(|i| i.viewport().clone());
        let Some(mon) = vp.monitor_size else {
            return;
        };
        if vp.maximized == Some(true) || vp.fullscreen == Some(true) {
            self.mon_clamped = true;
            return;
        }
        let Some(rect) = vp.inner_rect else {
            return;
        };
        // Reserve a strip for the taskbar/dock.
        let max_size = Vec2::new(mon.x, (mon.y - 48.0).max(240.0));
        let mut size = rect.size();
        let mut pos = rect.min;
        let mut changed = false;
        if size.x > max_size.x {
            size.x = max_size.x;
            changed = true;
        }
        if size.y > max_size.y {
            size.y = max_size.y;
            changed = true;
        }
        if pos.x < 0.0 {
            pos.x = 0.0;
            changed = true;
        }
        if pos.y < 0.0 {
            pos.y = 0.0;
            changed = true;
        }
        if pos.x + size.x > mon.x {
            pos.x = (mon.x - size.x).max(0.0);
            changed = true;
        }
        if pos.y + size.y > mon.y {
            pos.y = (mon.y - size.y).max(0.0);
            changed = true;
        }
        if changed {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        self.mon_clamped = true;
    }

    // ------------------------------------------------------- editing

    fn ops(&self, key: PageKey) -> Vec<EditOp> {
        self.edits.get(&key).cloned().unwrap_or_default()
    }

    /// Committed ops, plus the live slider values as a display-only tail.
    fn effective_ops(&self, key: PageKey) -> Vec<EditOp> {
        let mut ops = self.ops(key);
        let a = self.adjust;
        if a.brightness != 0 {
            ops.push(EditOp::Brightness(a.brightness));
        }
        if a.contrast != 0 {
            ops.push(EditOp::Contrast(a.contrast));
        }
        if a.saturation != 0 {
            ops.push(EditOp::Saturation(a.saturation));
        }
        ops
    }

    fn push_op(&mut self, op: EditOp) {
        let key = self.key();
        if self.cache.get(&key).map(|e| e.failed.is_some()).unwrap_or(true) {
            return;
        }
        self.edits.entry(key).or_default().push(op);
        if let Some(e) = self.cache.get_mut(&key) {
            e.edited = None;
            e.tex_sig = None;
            e.texture = None;
        }
    }

    fn undo(&mut self) {
        let key = self.key();
        let popped = self
            .edits
            .get_mut(&key)
            .map(|l| l.pop())
            .unwrap_or(None);
        if popped.is_some() {
            if let Some(e) = self.cache.get_mut(&key) {
                e.edited = None;
                e.tex_sig = None;
                e.texture = None;
            }
            if self.edits.get(&key).is_some_and(|l| l.is_empty()) {
                self.edits.remove(&key);
            }
        } else if !self.adjust.is_neutral() {
            self.adjust = Adjust::default();
        }
    }

    fn commit_adjust(&mut self) {
        let a = self.adjust;
        if !a.is_neutral() {
            let key = self.key();
            let list = self.edits.entry(key).or_default();
            if a.brightness != 0 {
                list.push(EditOp::Brightness(a.brightness));
            }
            if a.contrast != 0 {
                list.push(EditOp::Contrast(a.contrast));
            }
            if a.saturation != 0 {
                list.push(EditOp::Saturation(a.saturation));
            }
            if let Some(e) = self.cache.get_mut(&key) {
                e.edited = None;
                e.tex_sig = None;
                e.texture = None;
            }
        }
        self.adjust = Adjust::default();
    }

    fn current_image(&self) -> Option<Arc<image::DynamicImage>> {
        let key = self.key();
        let e = self.cache.get(&key)?;
        let o = e.original.as_ref()?;
        let ops = self.effective_ops(key);
        if ops.is_empty() {
            Some(o.clone())
        } else {
            Some(Arc::new(edit::apply_ops(o, &ops)))
        }
    }

    // ------------------------------------------------------- saving / clipboard

    fn save_dialog(&mut self) {
        let Some(src) = self.doc().and_then(|d| d.pages.get(self.cur_page)).cloned() else {
            return;
        };
        let Some(image) = self.current_image() else {
            return;
        };
        let name = Path::new(&src.describe())
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.png".into());
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Save As")
            .set_file_name(&name)
            .add_filter("Images", crate::archive::ALL_EXTS)
            .save_file()
        {
            match crate::save::save_as(&image, &path, self.jpeg_q.clamp(1.0, 100.0) as u8) {
                Ok(()) => self.flash = Some((format!("Saved {}", path.display()), Instant::now())),
                Err(e) => self.toast = Some((format!("Save failed: {e}"), Instant::now())),
            }
        }
    }

    fn save_overwrite(&mut self) {
        let Some(src) = self.doc().and_then(|d| d.pages.get(self.cur_page)).cloned() else {
            return;
        };
        let Some(image) = self.current_image() else { return };
        let path = match &src {
            doc::PageSource::File(p) => p.clone(),
            _ => {
                self.toast = Some((
                    "This page lives inside an archive/PDF — use Save As (Ctrl+Shift+S).".into(),
                    Instant::now(),
                ));
                return;
            }
        };
        let has_edits = !self.ops(self.key()).is_empty();
        match crate::save::overwrite(&image, &path, has_edits, self.jpeg_q.clamp(1.0, 100.0) as u8) {
            Ok(()) => {
                self.flash =
                    Some((format!("Saved {}", path.display()), Instant::now()))
            }
            Err(e) => self.toast = Some((format!("{e}"), Instant::now())),
        }
    }

    fn copy_to_clipboard(&mut self) {
        let Some(image) = self.current_image() else { return };
        let rgba = image.to_rgba8();
        match arboard::Clipboard::new().and_then(|mut cb| {
            cb.set_image(arboard::ImageData {
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                bytes: std::borrow::Cow::Owned(rgba.into_raw()),
            })
        }) {
            Ok(()) => self.flash = Some(("Copied to clipboard".into(), Instant::now())),
            Err(e) => self.toast = Some((format!("Clipboard failed: {e}"), Instant::now())),
        }
    }

    // ------------------------------------------------------- input

    fn handle_input(&mut self, ctx: &egui::Context) {
        let mut actions: Vec<Action> = Vec::new();
        let crop_mode = self.crop_mode;
        // Computed outside the input closure: egui 0.37 guards the whole
        // Context with one RwLock, so ctx.memory() nested inside ctx.input()
        // would deadlock.
        let no_focused_widget = ctx.memory(|m| m.focused().is_none());

        ctx.input(|i| {
            let ctrl = i.modifiers.ctrl;
            let shift = i.modifiers.shift;
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::Space) {
                actions.push(Action::Next);
            }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::Backspace) {
                actions.push(Action::Prev);
            }
            if i.key_pressed(Key::ArrowDown) && ctrl {
                actions.push(Action::NextDoc);
            }
            if i.key_pressed(Key::ArrowUp) && ctrl {
                actions.push(Action::PrevDoc);
            }
            if i.key_pressed(Key::PageDown) && !ctrl {
                actions.push(Action::NextDoc);
            }
            if i.key_pressed(Key::PageUp) && !ctrl {
                actions.push(Action::PrevDoc);
            }
            if i.key_pressed(Key::Home) {
                actions.push(Action::First);
            }
            if i.key_pressed(Key::End) {
                actions.push(Action::Last);
            }
            // When eframe hands us plain letters, keyboards may differ on layout.
            if i.key_pressed(Key::Equals) {
                actions.push(Action::ZoomIn);
            }
            if i.key_pressed(Key::Minus) {
                actions.push(Action::ZoomOut);
            }
            if i.key_pressed(Key::Num0) {
                actions.push(Action::Fit);
            }
            if i.key_pressed(Key::Num1) {
                actions.push(Action::Zoom100);
            }
            if i.key_pressed(Key::O) && ctrl && !shift {
                actions.push(Action::Open);
            }
            if i.key_pressed(Key::S) && ctrl {
                if shift {
                    actions.push(Action::SaveAs);
                } else {
                    actions.push(Action::Save);
                }
            }
            if i.key_pressed(Key::C) && ctrl {
                actions.push(Action::Copy);
            }
            if i.key_pressed(Key::Z) && ctrl {
                actions.push(Action::Undo);
            }
            if i.key_pressed(Key::R) && ctrl {
                actions.push(Action::RotateR);
            }
            if i.key_pressed(Key::L) && ctrl {
                actions.push(Action::RotateL);
            }
            if i.key_pressed(Key::H) && ctrl {
                actions.push(Action::FlipH);
            }
            if i.key_pressed(Key::V) && ctrl {
                actions.push(Action::FlipV);
            }
            if i.key_pressed(Key::F11) || (i.key_pressed(Key::F) && !ctrl) {
                actions.push(Action::Fullscreen);
            }
            if i.key_pressed(Key::A) && ctrl {
                actions.push(Action::Panel);
            }
            if i.key_pressed(Key::X) && (ctrl && shift) {
                actions.push(Action::Crop);
            }
            if i.key_pressed(Key::Enter) && crop_mode {
                actions.push(Action::CropApply);
            }
            if i.key_pressed(Key::Escape) && crop_mode {
                actions.push(Action::CropCancel);
            }
            if i.key_pressed(Key::Escape) && !crop_mode && self.fullscreen {
                actions.push(Action::Fullscreen);
            }
            if i.key_pressed(Key::Escape) && !crop_mode && !self.fullscreen && self.sliding {
                actions.push(Action::Slideshow);
            }
            // S starts/stops the slideshow (only when no text field has focus).
            if i.key_pressed(Key::S) && !ctrl && no_focused_widget {
                actions.push(Action::Slideshow);
            }
            let _ = shift;
        });

        for a in actions {
            self.do_action(ctx, a);
        }
    }

    fn do_action(&mut self, ctx: &egui::Context, a: Action) {
        match a {
            Action::Next => self.next_page(),
            Action::Prev => self.prev_page(),
            Action::NextDoc => self.next_doc(),
            Action::PrevDoc => self.prev_doc(),
            Action::First => {
                self.cur_page = 0;
                self.reset_view();
            }
            Action::Last => {
                self.cur_page = self.pages().saturating_sub(1);
                self.reset_view();
            }
            Action::ZoomIn => self.zoom_by(1.25, None, ctx),
            Action::ZoomOut => self.zoom_by(1.0 / 1.25, None, ctx),
            Action::Fit => self.reset_view(),
            Action::Zoom100 => self.enter_zoom100(ctx),
            Action::Open => self.open_dialog(),
            Action::Save => self.save_overwrite(),
            Action::SaveAs => self.save_dialog(),
            Action::Copy => self.copy_to_clipboard(),
            Action::Undo => self.undo(),
            Action::RotateL => self.push_op(EditOp::Rotate270),
            Action::RotateR => self.push_op(EditOp::Rotate90),
            Action::FlipH => self.push_op(EditOp::FlipH),
            Action::FlipV => self.push_op(EditOp::FlipV),
            Action::Grayscale => self.push_op(EditOp::Grayscale),
            Action::Sharpen => self.push_op(EditOp::Sharpen),
            Action::Crop => {
                self.crop_mode = !self.crop_mode;
                self.crop_rect = None;
                self.crop_start = None;
            }
            Action::CropApply => {
                if let Some(r) = self.crop_rect {
                    self.push_op(EditOp::Crop {
                        x: r.min.x.round() as i32,
                        y: r.min.y.round() as i32,
                        w: (r.width().round() as i32).max(1),
                        h: (r.height().round() as i32).max(1),
                    });
                    self.crop_mode = false;
                    self.crop_rect = None;
                }
            }
            Action::CropCancel => {
                self.crop_mode = false;
                self.crop_rect = None;
                self.crop_start = None;
            }
            Action::Panel => self.panel_open = !self.panel_open,
            Action::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                // Windows ghosts (stale window copies) and can flash black
                // while it resizes the window for fullscreen. Hide the window
                // for the ~100 ms the switch takes so none of that is visible.
                if self.fs_mask {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                if self.fs_mask {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                // The OS window resize arrives asynchronously; repaint for a
                // moment so no stale-geometry frame lingers on screen.
                self.repaint_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs_f32(0.8));
                // On entering, show the bars for the configured grace time;
                // on leaving, bring them back fully.
                if self.fullscreen {
                    self.chrome_until = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_secs_f32(self.chrome_hide_secs.max(0.0)),
                    );
                } else {
                    self.chrome_until = None;
                }
                self.chrome_anim = 1.0;
            }
            Action::Slideshow => {
                self.sliding = !self.sliding;
                if self.sliding {
                    let secs = self.slide_secs.clamp(0.5, 120.0);
                    self.slide_next = Some(Instant::now() + std::time::Duration::from_secs_f32(secs));
                    self.flash = Some((
                        format!("Slideshow on — every {:.0} s (S or Esc to stop)", secs),
                        Instant::now(),
                    ));
                } else {
                    self.slide_next = None;
                    self.flash = Some(("Slideshow off".into(), Instant::now()));
                }
            }
            Action::Exit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Zoom around `anchor` (screen coords), or around the view center.
    fn zoom_by(&mut self, factor: f32, anchor: Option<Pos2>, ctx: &egui::Context) {
        let avail = self.last_avail;
        let fit = self.fit_scale(avail.size(), ctx);
        let new_scale = match self.zoom {
            Zoom::Fit => {
                if factor < 1.0 {
                    return;
                }
                fit
            }
            Zoom::Scale(s) => s,
        };
        let s = (new_scale * factor).clamp(0.02, 40.0);
        if s <= fit * 1.02 {
            self.zoom = Zoom::Fit;
            self.pan = Vec2::ZERO;
            self.zoom_anim = None;
            return;
        }
        self.zoom = Zoom::Scale(s);
        // Keep the anchored point stable in image space.
        let a = anchor.unwrap_or_else(|| avail.center());
        let old_origin = self.view_origin(avail, ctx);
        let delta = a - old_origin;
        // After zooming, the point under `anchor` shifts by delta*(1 - new/old);
        // compensate via pan.
        let ratio = s / new_scale;
        self.pan += delta - delta * ratio;
        self.zoom_anim = None;
    }

    fn fit_scale(&self, avail: Vec2, ctx: &egui::Context) -> f32 {        let Some(tex) = self.best_texture_size(ctx) else {
            return 1.0;
        };
        if tex.x <= 0.0 || tex.y <= 0.0 {
            return 1.0;
        }
        let fx = avail.x / tex.x;
        let fy = avail.y / tex.y;
        match self.fit_mode {
            1 => fx,          // fit width
            2 => fy,          // fit height
            _ => fx.min(fy), // fit whole image
        }
    }

    fn best_texture_size(&self, _ctx: &egui::Context) -> Option<Vec2> {
        let key = self.key();
        let e = self.cache.get(&key)?;
        if let Some((_, img)) = e.edited.as_ref() {
            return Some(Vec2::new(img.width() as f32, img.height() as f32));
        }
        if let Some(img) = e.original.as_ref() {
            return Some(Vec2::new(img.width() as f32, img.height() as f32));
        }
        // The decoded CPU copy was dropped to save RAM and only the GPU
        // texture remains — use the dimensions recorded at decode time.
        e.dims.map(|(w, h)| Vec2::new(w as f32, h as f32))
    }

    /// Pan clamped per axis: center when the image fits, else keep an edge
    /// reachable (0 = left/top edge, negative = right/bottom content).
    fn clamp_pan(pan: Vec2, vp: Vec2, size: Vec2) -> Vec2 {
        let axis = |p: f32, vp_v: f32, size_v: f32| -> f32 {
            if size_v <= vp_v {
                (vp_v - size_v) / 2.0
            } else {
                p.clamp(vp_v - size_v, 0.0)
            }
        };
        Vec2::new(
            axis(pan.x, vp.x, size.x),
            axis(pan.y, vp.y, size.y),
        )
    }

    fn view_origin(&self, avail: Rect, ctx: &egui::Context) -> Pos2 {
        let Some(size_px) = self.best_texture_size(ctx) else {
            return avail.center();
        };
        let scale = match self.zoom {
            Zoom::Fit => self.fit_scale(avail.size(), ctx),
            Zoom::Scale(s) => s,
        };
        let size = Vec2::new(size_px.x * scale, size_px.y * scale);
        avail.min + Self::clamp_pan(self.pan, avail.size(), size)
    }

    // ------------------------------------------------------- prepared image

    fn prepare(&mut self, ctx: &egui::Context) -> Prepared {
        let key = self.key();

        // Nothing open at all: show the app-name splash instead of an error.
        if self.docs.is_empty() {
            return Prepared::Empty;
        }

        // A document with no pages (or a position outside it) can never be
        // decoded; say so instead of spinning forever.
        let has_page = self.doc().is_some_and(|d| self.cur_page < d.pages.len());
        if !has_page {
            let msg = self
                .doc()
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| "This document has no pages.".to_string());
            return Prepared::Failed(msg);
        }

        if !self.cache.contains_key(&key) {
            self.ensure_current();
            return Prepared::Loading;
        }
        let entry = self.cache.get(&key).unwrap();
        if let Some(f) = entry.failed.clone() {
            return Prepared::Failed(f);
        }

        let ops = self.effective_ops(key);
        let sig = edit::signature(&ops);

        let original = match entry.original.as_ref().map(Arc::clone) {
            Some(o) => Some(o),
            None => {
                // The original was dropped to keep RAM down. Without edits we
                // can show the cached texture as-is; with edits we re-decode.
                if ops.is_empty() {
                    if let Some(tex) = entry.texture.clone() {
                        return Prepared::Ready {
                            texture: tex,
                            edited: false,
                        };
                    }
                } else if let Some(src) = entry.source.clone() {
                    self.request(key, &src);
                }
                return Prepared::Loading;
            }
        };
        let original = original.unwrap();

        let image: Arc<image::DynamicImage> = if ops.is_empty() {
            original
        } else {
            let cached = entry
                .edited
                .as_ref()
                .is_some_and(|(s, _)| *s == sig);
            if cached {
                entry.edited.as_ref().map(|(_, i)| i.clone()).unwrap()
            } else {
                let edited = Arc::new(edit::apply_ops(&original, &ops));
                let entry = self.cache.get_mut(&key).unwrap();
                entry.edited = Some((sig.clone(), Arc::clone(&edited)));
                edited
            }
        };

        // (Re)build the GPU texture when the pixels changed.
        let entry = self.cache.get_mut(&key).unwrap();
        let tex_sig = format!("{sig}:{}", if self.smooth_scale { "s" } else { "n" });
        if entry.tex_sig.as_deref() != Some(tex_sig.as_str()) || entry.texture.is_none()
        {
            let rgba = image.to_rgba8();
            let col = ColorImage::from_rgba_unmultiplied(
                [rgba.width() as usize, rgba.height() as usize],
                rgba.into_raw().as_slice(),
            );
            let tex = ctx.load_texture(
                "lumo-page",
                col,
                if self.smooth_scale {
                    egui::TextureOptions::LINEAR
                } else {
                    egui::TextureOptions::NEAREST
                },
            );
            entry.texture = Some(tex);
            entry.tex_sig = Some(tex_sig);
        }

        Prepared::Ready {
            texture: entry.texture.clone().unwrap(),
            edited: !ops.is_empty(),
        }
    }

    // ------------------------------------------------------- panels

    /// Proper top bar: menu row (File / View / Edit / ⚙) over a slim row of
    /// vector-icon buttons for frequent actions; file info in the center.
    /// Smoothed bar visibility for animation (0..1), windowed mode is always 1.
    fn chrome_vis(&self) -> f32 {
        if !self.fullscreen {
            return 1.0;
        }
        let p = self.chrome_anim.clamp(0.0, 1.0);
        p * p * (3.0 - 2.0 * p) // smoothstep
    }

    /// View background color from the stored 0xRRGGBB value.
    fn bg_color(v: u32) -> Color32 {
        Color32::from_rgb(((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8)
    }

    fn topbar(&mut self, ui: &mut egui::Ui) {
        if self.fullscreen {
            // Overlay, not a panel: the image keeps the whole window, so it
            // never rescales when the bars appear or hide. The bar floats
            // above the image and slides out through the top edge.
            let vis = self.chrome_vis();
            if vis <= 0.005 {
                return;
            }
            let style = ui.style().clone();
            let slide = (self.top_full_h + 2.0) * (1.0 - vis);
            let win_w = ui.ctx().input(|i| i.viewport_rect().width());
            Area::new(egui::Id::unique("lumo-fs-top"))
                .anchor(Align2::CENTER_TOP, [0.0, -slide])
                .show(ui.ctx(), |ui| {
                    ui.set_min_width(win_w);
                    Frame::central_panel(&style).show(ui, |ui| self.topbar_body(ui));
                });
        } else {
            let r = Panel::top("lumo-topbar").show(ui, |ui| self.topbar_body(ui));
            // Remember the natural height so the fullscreen animation
            // starts from the right size.
            let h = r.response.rect.height();
            if h > 10.0 {
                self.top_full_h = h;
            }
        }
    }

    fn topbar_body(&mut self, ui: &mut egui::Ui) {
        {
            ui.add_space(2.0);

            // ---- row 1: menu bar + centered file info + settings on the right
            let row = ui.horizontal(|ui| {
                let ctx = ui.ctx().clone();
                let mut act = |ui: &mut egui::Ui, a: Action| {
                    if ui.button(Self::menu_label(a)).clicked() {
                        self.do_action(&ctx, a);
                    }
                };

                ui.menu_button("File", |ui| {
                    act(ui, Action::Open);
                    act(ui, Action::Save);
                    act(ui, Action::SaveAs);
                    act(ui, Action::Copy);
                    ui.separator();
                    act(ui, Action::Exit);
                });
                ui.menu_button("View", |ui| {
                    act(ui, Action::Fit);
                    act(ui, Action::Zoom100);
                    act(ui, Action::Panel);
                    ui.separator();
                    act(ui, Action::Slideshow);
                    act(ui, Action::Fullscreen);
                });
                ui.menu_button("Edit", |ui| {
                    act(ui, Action::RotateL);
                    act(ui, Action::RotateR);
                    act(ui, Action::FlipH);
                    act(ui, Action::FlipV);
                    ui.separator();
                    act(ui, Action::Crop);
                    act(ui, Action::Grayscale);
                    act(ui, Action::Sharpen);
                    ui.separator();
                    act(ui, Action::Undo);
                });

            let info_left = ui.cursor().min.x;

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::containers::menu::MenuButton::new("⚙")
                        .config(
                            egui::containers::menu::MenuConfig::default()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, |ui| {
                            // Stays open while you click inside, so every
                            // option can be changed in one visit.
                            egui::ScrollArea::vertical().max_height(460.0).show(ui, |ui| {
                                ui.collapsing("Reading", |ui| {
                                    let wp = ui.checkbox(
                                        &mut self.wheel_pages,
                                        "Mouse wheel turns pages",
                                    );
                                    if wp.changed() {
                                        self.save_settings();
                                    }
                                    wp.on_hover_text(
                                        "On: the wheel flips to the next/previous page (one notch =\n\
                                         one page) and Ctrl+wheel zooms.\n\
                                         Off: the wheel zooms.",
                                    );
                                    ui.label(
                                        egui::RichText::new("Wheel zoom per notch").weak().size(12.0),
                                    );
                                    let zr = ui
                                        .add(
                                            egui::DragValue::new(&mut self.wheel_zoom_pct)
                                                .speed(0.5)
                                                .range(1.0..=80.0)
                                                .suffix("%"),
                                        )
                                        .on_hover_text(
                                            "How much one scroll-wheel notch zooms in/out.\n\
                                             Drag sideways to change.",
                                        );
                                    if zr.changed() {
                                            self.save_settings();
                                        }
                                    let rz = ui.checkbox(
                                        &mut self.remember_zoom,
                                        "Remember zoom between pages",
                                    );
                                    if rz.changed() {
                                        self.save_settings();
                                    }
                                    rz.on_hover_text(
                                        "On: flipping pages keeps your zoom level and pan position.\n\
                                         Off: every page opens fitted to the window.",
                                    );
                                    ui.label(egui::RichText::new("Fit mode").weak().size(12.0));
                                    let mut fm = self.fit_mode.clamp(0, 2);
                                    let f0 = ui.radio_value(&mut fm, 0, "Whole image");
                                    let f1 = ui.radio_value(&mut fm, 1, "Width");
                                    let f2 = ui.radio_value(&mut fm, 2, "Height");
                                    if fm != self.fit_mode {
                                        self.fit_mode = fm;
                                        self.save_settings();
                                    }
                                    let _ = (f0, f1, f2);
                                    ui.label(
                                        egui::RichText::new("Fit width / height suits tall comic pages.")
                                            .weak()
                                            .size(11.0),
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("Auto-scroll speed");
                                        let aspd = ui
                                            .add(
                                                egui::DragValue::new(&mut self.autoscroll_speed)
                                                    .speed(0.05)
                                                    .range(0.05..=3.0)
                                                    .suffix(" ×/s"),
                                            )
                                            .on_hover_text(
                                                "Speed of the steady scroll when you HOLD ↓ or ↑\n\
                                                 (works when the page is taller than the window,\n\
                                                 e.g. fit-width). Value is in view-heights per second.",
                                            );
                                        if aspd.changed() {
                                            self.save_settings();
                                        }
                                    });
                                    let lm = ui.checkbox(&mut self.low_memory, "Low memory mode");
                                    if lm.changed() {
                                        self.save_settings();
                                    }
                                    lm.on_hover_text(
                                        "Keep only the page you are on and its immediate\n\
                                         neighbours decoded. Everything else is dropped as you\n\
                                         leave it, so memory stays flat no matter how long the\n\
                                         queue is. Flipping back far re-decodes (a brief spinner).",
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("RAM cache");
                                        let cm = ui
                                            .add(
                                                egui::DragValue::new(&mut self.cache_mb)
                                                    .speed(8.0)
                                                    .range(32.0..=2048.0)
                                                    .suffix(" MB"),
                                            )
                                            .on_hover_text(
                                                "How many decoded pages to keep around. Lower = less\n\
                                                 RAM, more re-decoding when you flip back.",
                                            );
                                        if cm.changed() {
                                            self.save_settings();
                                        }
                                    });
                                });
                                ui.collapsing("Fullscreen", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label("Hide bars after");
                                        let hs = ui
                                            .add(
                                                egui::DragValue::new(&mut self.chrome_hide_secs)
                                                    .speed(0.1)
                                                    .range(0.0..=10.0)
                                                    .suffix(" s"),
                                            )
                                            .on_hover_text(
                                                "How long the menu/icon bars stay visible in\n\
                                                 fullscreen after the mouse last moved.\n\
                                                 0 = hide immediately (the slide-out animation\n\
                                                 still plays).",
                                            );
                                        if hs.changed() {
                                            self.save_settings();
                                        }
                                    });
                                    let fm = ui.checkbox(
                                        &mut self.fs_mask,
                                        "Clean switch (hide window during the change)",
                                    );
                                    if fm.changed() {
                                        self.save_settings();
                                    }
                                    fm.on_hover_text(
                                        "Windows shows ghosting or a black flash while it resizes\n\
                                         the window for fullscreen. With this on, lumo hides the\n\
                                         window for the moment the switch takes, so the change\n\
                                         looks like a clean blink. Uncheck if the keyboard seems\n\
                                         unresponsive after a switch (clicking fixes focus).",
                                    );
                                    let df = ui.checkbox(
                                        &mut self.dbl_fs,
                                        "Double-click toggles fullscreen",
                                    );
                                    if df.changed() {
                                        self.save_settings();
                                    }
                                    df.on_hover_text(
                                        "On: double-clicking the image enters or leaves fullscreen.\n\
                                         Off: double-clicking switches between fit and 100%.\n\
                                         (Keys 0 / 1 still switch fit and 100% either way.)",
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("Hide mouse cursor after");
                                        let hc = ui
                                            .add(
                                                egui::DragValue::new(&mut self.hide_cursor_secs)
                                                    .speed(0.1)
                                                    .range(0.0..=10.0)
                                                    .suffix(" s"),
                                            )
                                            .on_hover_text(
                                                "Fullscreen only: fade the mouse pointer away after\n\
                                                 this many seconds without moving. 0 = keep visible.\n\
                                                 Moving the mouse brings it (and the bars) back.",
                                            );
                                        if hc.changed() {
                                            self.save_settings();
                                        }
                                    });
                                });
                                ui.collapsing("Slideshow", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label("Advance every");
                                        let ss = ui
                                            .add(
                                                egui::DragValue::new(&mut self.slide_secs)
                                                    .speed(0.5)
                                                    .range(0.5..=120.0)
                                                    .suffix(" s"),
                                            )
                                            .on_hover_text(
                                                "Time each page stays on screen in the slideshow.\n\
                                                 Start/stop with S or the View menu.",
                                            );
                                        if ss.changed() {
                                            self.save_settings();
                                        }
                                    });
                                    let sl = ui.checkbox(&mut self.slide_loop, "Loop back to the start");
                                    if sl.changed() {
                                        self.save_settings();
                                    }
                                    sl.on_hover_text(
                                        "On: the slideshow restarts from the first page after the\n\
                                         last. Off: it stops at the end.",
                                    );
                                    ui.label(
                                        egui::RichText::new("S starts/stops · Esc stops").weak().size(11.0),
                                    );
                                });
                                ui.collapsing("Look", |ui| {
                                    let th = ui.checkbox(&mut self.theme_light, "Light theme");
                                    if th.changed() {
                                        self.save_settings();
                                        ui.ctx().set_theme(if self.theme_light {
                                            egui::ThemePreference::Light
                                        } else {
                                            egui::ThemePreference::Dark
                                        });
                                    }
                                    ui.horizontal(|ui| {
                                        ui.label("View background");
                                        let mut col = Self::bg_color(self.view_bg);
                                        let ce = ui.color_edit_button_srgba(&mut col);
                                        if ce.changed() {
                                            self.view_bg = ((col.r() as u32) << 16)
                                                | ((col.g() as u32) << 8)
                                                | (col.b() as u32);
                                            self.save_settings();
                                        }
                                        ce.on_hover_text(
                                            "The area around the image (page gaps and borders).",
                                        );
                                    });
                                    let sm = ui.checkbox(
                                        &mut self.smooth_scale,
                                        "Smooth (antialiased) scaling",
                                    );
                                    if sm.changed() {
                                        self.save_settings();
                                    }
                                    sm.on_hover_text(
                                        "On: soft, smooth rendering.\n\
                                         Off: pixelated 1:1 pixels — crisp for manga text at high zoom.",
                                    );
                                });
                                ui.collapsing("Startup & files", |ui| {
                                    let rw = ui.checkbox(
                                        &mut self.remember_win,
                                        "Remember window size and position",
                                    );
                                    if rw.changed() {
                                        self.save_settings();
                                    }
                                    rw.on_hover_text(
                                        "Saved when you close lumo; restored on the next launch.",
                                    );
                                    let sf = ui.checkbox(&mut self.start_fs, "Start in fullscreen");
                                    if sf.changed() {
                                        self.save_settings();
                                    }
                                    sf.on_hover_text("lumo opens in fullscreen next launch.");
                                    let rl = ui.checkbox(
                                        &mut self.reopen_last,
                                        "Reopen last files at launch",
                                    );
                                    if rl.changed() {
                                        self.save_settings();
                                    }
                                    rl.on_hover_text(
                                        "When lumo is started without files, it reopens what you\n\
                                         had open when you last closed it.",
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("JPEG save quality");
                                        let jq = ui
                                            .add(
                                                egui::DragValue::new(&mut self.jpeg_q)
                                                    .speed(1.0)
                                                    .range(1.0..=100.0),
                                            )
                                            .on_hover_text(
                                                "Quality used when saving .jpg/.jpeg files\n\
                                                 (Save / Save As). 92 is a good default.",
                                            );
                                        if jq.changed() {
                                            self.save_settings();
                                        }
                                    });
                                    ui.label(egui::RichText::new("Sort opened files").weak().size(12.0));
                                    let mut so = self.sort_mode.clamp(0, 4);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.radio_value(&mut so, 0, "Selection order");
                                        ui.radio_value(&mut so, 1, "Name A→Z");
                                        ui.radio_value(&mut so, 2, "Name Z→A");
                                        ui.radio_value(&mut so, 3, "Newest");
                                        ui.radio_value(&mut so, 4, "Oldest");
                                    });
                                    if so != self.sort_mode {
                                        self.sort_mode = so;
                                        self.save_settings();
                                    }
                                    ui.label(
                                        egui::RichText::new("Applies to files opened after the change; name sorting is natural (img2 before img10).")
                                            .weak()
                                            .size(11.0),
                                    );
                                });
                            });
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!(
                                    "lumo v{} — image viewer & comic reader",
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .weak()
                                .size(12.0),
                            );
                        });
                });

                info_left
            });
            let row_rect = row.response.rect;
            let info_left = row.inner;

            // centered file label, drawn directly so it survives narrow windows
            let info = self.file_info_text();
            if !info.is_empty() {
                let weak = Color32::from_rgba_unmultiplied(185, 185, 185, 255);
                let info_right = row_rect.max.x - 48.0;
                if info_right > info_left {
                    let c = Pos2::new(
                        (info_left + info_right) * 0.5,
                        row_rect.center().y,
                    );
                    ui.painter().text(
                        c,
                        Align2::CENTER_CENTER,
                        info,
                        egui::FontId::proportional(13.5),
                        weak,
                    );
                }
            }

            // ---- row 2: slim icon strip
            ui.horizontal(|ui| {
                ui.add_space(2.0);
                // (icon buttons are a method on Self below — borrowing self
                // only per call, so plain button actions can interleave)

                // One pair of arrows does both jobs: within a multi-page album
                // they step page by page, and at the ends they cross into the
                // neighbouring file. PgUp/PgDn jump files directly.
                self.icon_btn(ui, icons::Icon::Prev, "Previous (← / Space; PgUp = previous file)", Action::Prev);
                self.icon_btn(ui, icons::Icon::Next, "Next (→ / Space; PgDn = next file)", Action::Next);
                ui.separator();

                // View mode: shows the current mode, click to switch.
                let mode_label: String = match self.zoom {
                    Zoom::Fit => "fit".into(),
                    Zoom::Scale(s) if (s - 1.0).abs() < 1e-3 => "1:1".into(),
                    Zoom::Scale(s) => format!("{}%", (s * 100.0).round() as i32),
                };
                let mode_tip = if matches!(self.zoom, Zoom::Fit) {
                    "View: fit to window — click to switch to 100% (0 / 1)"
                } else {
                    "View: 100% — click to switch to fit (0 / 1)"
                };
                if ui
                    .add_sized([46.0, 22.0], egui::Button::new(mode_label.as_str()).small())
                    .on_hover_text(mode_tip)
                    .clicked()
                {
                    if matches!(self.zoom, Zoom::Fit) {
                        self.do_action(ui.ctx(), Action::Zoom100);
                    } else {
                        self.do_action(ui.ctx(), Action::Fit);
                    }
                }
                ui.separator();
                self.icon_btn(ui, icons::Icon::RotateL, "Rotate left (Ctrl+L)", Action::RotateL);
                self.icon_btn(ui, icons::Icon::RotateR, "Rotate right (Ctrl+R)", Action::RotateR);
                self.icon_btn(ui, icons::Icon::FlipH, "Flip horizontal (Ctrl+H)", Action::FlipH);
                self.icon_btn(ui, icons::Icon::FlipV, "Flip vertical (Ctrl+V)", Action::FlipV);
                self.icon_btn_active(
                    ui,
                    icons::Icon::Crop,
                    "Crop (Ctrl+X): drag a box on the image, Enter applies",
                    Action::Crop,
                    self.crop_mode,
                );
                ui.separator();
                self.icon_btn(ui, icons::Icon::Undo, "Undo (Ctrl+Z)", Action::Undo);
                self.icon_btn(ui, icons::Icon::Open, "Open files (Ctrl+O)", Action::Open);
                self.icon_btn(ui, icons::Icon::Save, "Save (Ctrl+S)", Action::Save);
            });
            ui.add_space(2.0);
        }
    }

    /// One 26-px vector-icon button in the slim actions row. Borrows `self`
    /// only for the call, so it may be interleaved with other `self` uses.
    fn icon_btn(&mut self, ui: &mut egui::Ui, icon: icons::Icon, tip: &str, action: Action) {
        self.icon_btn_active(ui, icon, tip, action, false);
    }

    /// Same button, but `selected` pins the hover shade on so the user can
    /// see the mode is active (crop while a selection is being drawn).
    fn icon_btn_active(
        &mut self,
        ui: &mut egui::Ui,
        icon: icons::Icon,
        tip: &str,
        action: Action,
        selected: bool,
    ) {
        let size = egui::Vec2::splat(26.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let hot = resp.hovered() || resp.is_pointer_button_down_on();
        if selected || hot {
            ui.painter().rect_filled(
                rect.expand2(egui::vec2(2.0, 1.0)),
                4.0,
                ui.visuals().widgets.hovered.bg_fill,
            );
        }
        let fg = if selected || hot {
            ui.visuals().widgets.hovered.fg_stroke.color
        } else {
            ui.visuals().widgets.inactive.fg_stroke.color
        };
        icons::paint(ui.painter(), rect.shrink2(egui::Vec2::splat(3.5)), icon, fg);
        let resp = resp.on_hover_text(tip);
        if resp.clicked() {
            self.do_action(ui.ctx(), action);
        }
    }

    /// Menu item label: action name + its keyboard shortcut.
    fn menu_label(action: Action) -> &'static str {
        match action {
            Action::Open => "Open…  (Ctrl+O)",
            Action::Save => "Save  (Ctrl+S)",
            Action::SaveAs => "Save As…  (Ctrl+Shift+S)",
            Action::Copy => "Copy image  (Ctrl+C)",
            Action::Exit => "Exit  (Alt+F4)",
            Action::Fit => "Fit  (0)",
            Action::Zoom100 => "100%  (1)",
            Action::Panel => "Adjust panel  (P)",
            Action::Fullscreen => "Fullscreen  (F11)",
            Action::Slideshow => "Slideshow  (S)",
            Action::RotateL => "Rotate left  (Ctrl+L)",
            Action::RotateR => "Rotate right  (Ctrl+R)",
            Action::FlipH => "Flip horizontal  (Ctrl+H)",
            Action::FlipV => "Flip vertical  (Ctrl+V)",
            Action::Crop => "Crop  (Ctrl+X)",
            Action::Grayscale => "Black & white",
            Action::Sharpen => "Sharpen",
            Action::Undo => "Undo  (Ctrl+Z)",
            _ => "???",
        }
    }

    /// File identity shown centered in the menu row.
    fn file_info_text(&self) -> String {
        match self.doc() {
            Some(d) => {
                let pos = format!(
                    "{} · file {}/{} · page {}/{}",
                    d.label,
                    self.cur_doc + 1,
                    self.docs.len(),
                    self.cur_page + 1,
                    self.pages().max(1),
                );
                match d.error {
                    Some(ref e) => format!("{pos} — {e}"),
                    None => pos,
                }
            }
            None => String::new(),
        }
    }

    fn edit_panel(&mut self, ui: &mut egui::Ui) {
        if !self.panel_open {
            return;
        }
        Panel::right("lumo-adjust").show(ui, |ui| {
            ui.heading("Adjust");
            ui.add_space(6.0);
            let mut commit = false;
            let b = ui.add(
                egui::Slider::new(&mut self.adjust.brightness, -100..=100).text("Brightness"),
            );
            commit |= b.drag_stopped();
            let c = ui.add(
                egui::Slider::new(&mut self.adjust.contrast, -100..=100).text("Contrast"),
            );
            commit |= c.drag_stopped();
            let s = ui.add(
                egui::Slider::new(&mut self.adjust.saturation, -100..=100).text("Saturation"),
            );
            commit |= s.drag_stopped();
            if commit {
                self.commit_adjust();
            }
            ui.add_space(8.0);
            if ui.button("Apply values").clicked() {
                self.commit_adjust();
            }
            if ui.button("Reset sliders").clicked() {
                self.adjust = Adjust::default();
            }
            ui.add_space(12.0);
            if ui.button("Sharpen").clicked() {
                self.do_action(ui.ctx(), Action::Sharpen);
            }
            ui.add_space(12.0);
            ui.heading("Resize");
            let mut w = 0;
            let mut h = 0;
            if let Some(img) = self.current_image() {
                w = img.width() as i32;
                h = img.height() as i32;
            }
            ui.horizontal(|ui| {
                ui.label("W");
                ui.add(egui::DragValue::new(&mut w).speed(1.0).range(1..=999999));
            });
            ui.horizontal(|ui| {
                ui.label("H");
                ui.add(egui::DragValue::new(&mut h).speed(1.0).range(1..=999999));
            });
            if ui.button("Apply resize").clicked() && w > 0 && h > 0 {
                self.push_op(EditOp::Resize {
                    w: w as u32,
                    h: h as u32,
                });
            }
        });
    }

    fn status(&mut self, ui: &mut egui::Ui) {
        if self.fullscreen {
            // Overlay, same as the top bar: floats above the image, slides
            // down out of view.
            let vis = self.chrome_vis();
            if vis <= 0.005 {
                return;
            }
            let style = ui.style().clone();
            let slide = (self.status_full_h + 2.0) * (1.0 - vis);
            let win_w = ui.ctx().input(|i| i.viewport_rect().width());
            Area::new(egui::Id::unique("lumo-fs-status"))
                .anchor(Align2::CENTER_BOTTOM, [0.0, slide])
                .show(ui.ctx(), |ui| {
                    ui.set_min_width(win_w);
                    Frame::central_panel(&style).show(ui, |ui| self.status_body(ui));
                });
        } else {
            let r = Panel::bottom("lumo-status").show(ui, |ui| self.status_body(ui));
            let h = r.response.rect.height();
            if h > 10.0 {
                self.status_full_h = h;
            }
        }
    }

    fn status_body(&mut self, ui: &mut egui::Ui) {
        {
            ui.horizontal(|ui| {
                // Left side: file & page identity
                let location = match self.doc() {
                    Some(d) => {
                        let pos = format!(
                            "file {}/{} · page {}/{}",
                            self.cur_doc + 1,
                            self.docs.len(),
                            self.cur_page + 1,
                            self.pages().max(1),
                        );
                        match d.error {
                            Some(ref e) => format!("{} — {e}", d.label),
                            None if self.pages() == 0 => format!("{} — (empty)", d.label),
                            None => {
                                let src = d.pages.get(self.cur_page);
                                format!(
                                    "{pos} — {}",
                                    src.map(|s| s.describe()).unwrap_or_default()
                                )
                            }
                        }
                    }
                    None => "Drop files here, or press Ctrl+O to open".to_string(),
                };
                let _ = location;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Lazy mutable state cleanup
                    if let Some((m, t)) = self.flash.clone() {
                        if t.elapsed().as_secs_f32() < 3.0 {
                            ui.label(m);
                        }
                    }
                    if let Some((m, t)) = self.toast.clone() {
                        if t.elapsed().as_secs_f32() < 6.0 {
                            ui.colored_label(Color32::LIGHT_RED, m);
                        }
                    }
                    if let Some(e) = self.cache.get(&self.key()) {
                        if let Some(f) = &e.failed {
                            ui.colored_label(Color32::RED, f);
                        }
                    }
                    let zoom_label = match self.zoom {
                        Zoom::Fit => "fit".to_string(),
                        Zoom::Scale(s) => format!("{}%", (s * 100.0).round() as i32),
                    };
                    let edits_suffix = if self.edits.contains_key(&self.key()) {
                        format!(
                            " · {} edits",
                            self.edits.get(&self.key()).map(|v| v.len()).unwrap_or(0)
                        )
                    } else {
                        String::new()
                    };
                    ui.label(
                        egui::RichText::new(format!("{zoom_label}{edits_suffix}"))
                            .weak()
                            .monospace(),
                    )
                    .on_hover_text(format!(
                        "current zoom · each wheel notch steps {:.0}%",
                        self.wheel_zoom_pct.clamp(1.0, 80.0).round()
                    ));
                });
            });

            // clean expired state notifications
            if let Some((_, t)) = self.flash.clone() {
                if t.elapsed().as_secs_f32() >= 3.0 {
                    self.flash = None;
                }
            }
            if let Some((_, t)) = self.toast.clone() {
                if t.elapsed().as_secs_f32() >= 6.0 {
                    self.toast = None;
                }
            }
        }
    }

    // ------------------------------------------------------- image view

    fn view_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        // A viewport resize (entering/leaving fullscreen, dragging an edge) can
        // leave one or more frames painted with the old geometry — stretched or
        // torn content. Don't carry a zoom animation through the size change,
        // and keep repainting briefly so the settled frame shows fast.
        if (avail.size() - self.last_view_size).length() > 0.5 {
            self.last_view_size = avail.size();
            self.zoom_anim = None;
            self.repaint_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs_f32(0.6));
        }
        self.last_avail = avail;
        let resp = ui.allocate_rect(avail, egui::Sense::click_and_drag());
        let ctx = ui.ctx().clone();

        // View background (configurable; also carries the theme).
        ui.painter().rect_filled(avail, 0.0, Self::bg_color(self.view_bg));

        let prepared = self.prepare(&ctx);
        let mut repainted_hints = false;
        let _ = &mut repainted_hints;

        match prepared {
            Prepared::Empty => {
                // Idle opening: just the app name, big, bold and grey.
                let band = Rect::from_center_size(
                    avail.center(),
                    Vec2::new(avail.width(), 120.0),
                );
                ui.put(
                    band,
                    egui::Label::new(
                        egui::RichText::new("lumo")
                            .strong()
                            .size(88.0)
                            .color(Color32::from_rgb(150, 150, 150)),
                    )
                    .halign(egui::Align::Center)
                    .selectable(false),
                );
            }
            Prepared::Loading => {
                let center = avail.center();
                ui.put(
                    Rect::from_center_size(center - Vec2::new(0.0, 14.0), Vec2::splat(56.0)),
                    egui::Spinner::new().size(56.0),
                );
                ui.put(
                    Rect::from_center_size(center + Vec2::new(0.0, 48.0), Vec2::new(320.0, 22.0)),
                    egui::Label::new(egui::RichText::new("Loading…").size(14.0).weak()),
                );
                // keep polling the decode thread
                ctx.request_repaint();
            }
            Prepared::Failed(msg) => {
                ui.put(
                    avail,
                    egui::Label::new(
                        egui::RichText::new(format!("⚠ {msg}"))
                            .color(Color32::LIGHT_RED)
                            .size(15.0),
                    )
                    .wrap(),
                );
            }
            Prepared::Ready { texture, .. } => {
                let key = self.key();
                let img_size = self
                    .best_texture_size(&ctx)
                    .unwrap_or_else(|| Vec2::splat(1.0));
                let fit = self.fit_scale(avail.size(), &ctx);
                // Advance the smooth-zoom animation, then use displayed values.
                self.zoom_tick(&ctx);
                let (scale, mut pan) = self.zoom_display(fit);

                // Clamp pan per axis: when the image fits, center it; when it
                // overflows, keep an edge reachable (0 = left/top, negative =
                // right/bottom content).
                let size = Vec2::new(img_size.x * scale, img_size.y * scale);
                pan = Self::clamp_pan(pan, avail.size(), size);
                if self.zoom_anim.is_none() {
                    self.pan = pan;
                }
                let origin = avail.min + pan;
                let rect = Rect::from_min_size(origin, size);
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                ui.painter()
                    .image(texture.id(), rect, uv, Color32::WHITE);

                // ---- wheel zoom (raw events, proportional & consistent).
                // Gate on the pointer being over the image rect, NOT on
                // `resp.hovered()`: on Windows the later-registered edge-arrow
                // click zones can invalidate the response hover, which made
                // the wheel dead.
                let over_img = ctx
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(|p| avail.contains(p));
                if over_img {
                    // Sum the raw wheel events of this frame, normalized to lines.
                    // (egui's smooth_scroll_delta keeps decaying for several
                    // frames after the wheel stops, which made zoom erratic.)
                    let wheel_lines = ctx.input(|i| {
                        i.raw.events.iter().filter_map(|e| match e {
                            egui::Event::MouseWheel { unit, delta, .. } => Some(match unit {
                                egui::MouseWheelUnit::Line => delta.y,
                                egui::MouseWheelUnit::Point => delta.y / 16.0,
                                egui::MouseWheelUnit::Page => delta.y * 16.0,
                            }),
                            _ => None,
                        })
                        .sum::<f32>()
                    });
                    // Wheel = page turning (setting); Ctrl+wheel still zooms.
                    // A "burst" = the frames while wheel events keep coming;
                    // when a frame brings none, the burst ends and exactly
                    // one page turn happens (one notch = one page, whether
                    // the OS sends 1 or 2 raw events per notch). A long
                    // touchpad gesture flips at most every ~8 lines.
                    let ctrl = ctx.input(|i| i.modifiers.ctrl);
                    if self.wheel_pages && !ctrl && !self.crop_mode {
                        if wheel_lines.abs() > 0.01 {
                            self.wheel_page_acc += wheel_lines;
                            if self.wheel_page_acc.abs() >= 8.0 {
                                let next = self.wheel_page_acc < 0.0;
                                self.wheel_page_acc = 0.0;
                                self.do_action(&ctx, if next { Action::Next } else { Action::Prev });
                            }
                        } else if self.wheel_page_acc != 0.0 {
                            let next = self.wheel_page_acc < 0.0;
                            self.wheel_page_acc = 0.0;
                            self.do_action(&ctx, if next { Action::Next } else { Action::Prev });
                        }
                    } else if wheel_lines.abs() > 0.01 {
                        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                            self.zoom_wheel(&ctx, wheel_lines, p);
                        }
                    }
                }

                // ---- hold ↓/↑ to scroll steadily (autoscroll)
                if self.autoscroll_speed > 0.01 && !self.crop_mode {
                    let down = ctx.input(|i| i.key_down(Key::ArrowDown));
                    let up = ctx.input(|i| i.key_down(Key::ArrowUp));
                    if down || up {
                        let dir: f32 = if down { 1.0 } else { -1.0 };
                        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.05);
                        let v = self.autoscroll_speed.clamp(0.05, 5.0) * avail.size().y * dt;
                        // In Fit mode the painted pan is zero by definition —
                        // materialize the current fit scale so panning applies.
                        if self.zoom == Zoom::Fit {
                            self.zoom = Zoom::Scale(self.fit_scale(avail.size(), &ctx));
                        }
                        self.pan.y -= dir * v;
                        self.zoom_anim = None;
                        ctx.request_repaint();
                    }
                }

                // ---- drag pan (content follows the cursor)
                if resp.dragged() && !self.crop_mode {
                    self.pan += resp.drag_delta();
                    self.zoom_anim = None;
                }

                // ---- double click: fullscreen toggle (setting), else fit ↔ 100%
                if !self.crop_mode && resp.double_clicked() {
                    if self.dbl_fs {
                        self.do_action(&ctx, Action::Fullscreen);
                    } else if self.zoom == Zoom::Scale(1.0) {
                        self.reset_view();
                    } else {
                        self.enter_zoom100(&ctx);
                    }
                }

                // ---- edge hover arrows (appear at the window ends only
                //      when there is a previous/next page and the cursor
                //      is near that end)
                if !self.crop_mode {
                    self.edge_arrows(ui, &ctx, avail);
                }

                // ---- overlay scrollbars when the image overflows
                if !self.crop_mode {
                    self.scrollbars(ui, avail, size);
                }

                // ---- crop
                if self.crop_mode {
                    self.crop_ui(ui, &ctx, rect, &resp, img_size);
                } else {
                    self.crop_start = None;
                    self.crop_rect = None;
                }
                let _ = key;
            }
        }
    }

    /// Big soft-edge chevron arrows at the window ends: a gentle gradient
    /// strip that fades out toward the image, an angular chevron in the
    /// flaticon "right arrow" style, and a quick fade + slide animation
    /// when it appears on hover. Only drawn while the cursor hovers the
    /// strip, and only when a previous/next page exists in the selection.
    /// Overlay scrollbars (right = vertical, bottom = horizontal) shown only
    /// when the zoomed image overflows the viewport. Dragging a thumb pans
    /// the image; they are painted last so they win interaction over the
    /// pan gesture underneath.
    fn scrollbars(&mut self, ui: &mut egui::Ui, avail: Rect, content: Vec2) {
        let vp = avail.size();
        if content.x <= vp.x && content.y <= vp.y {
            return;
        }
        let hover_col = ui.visuals().widgets.hovered.fg_stroke.color;
        let idle_col = Color32::from_white_alpha(90);
        let p = ui.painter();

        let v_show = content.y > vp.y + 1.0;
        let h_show = content.x > vp.x + 1.0;

        if v_show {
            let track = Rect::from_min_max(
                Pos2::new(avail.max.x - 12.0, avail.min.y + 2.0),
                Pos2::new(avail.max.x - 4.0, avail.max.y - if h_show { 12.0 } else { 2.0 }),
            );
            let resp = ui.interact(track, egui::Id::unique("lumo-vscroll"), egui::Sense::drag());
            let frac = (vp.y / content.y).clamp(0.05, 1.0);
            let thumb_len = track.height() * frac;
            let scrollable = content.y - vp.y;
            let t = (-self.pan.y / scrollable).clamp(0.0, 1.0);
            let thumb = Rect::from_min_size(
                Pos2::new(track.min.x, track.min.y + t * (track.height() - thumb_len)),
                Vec2::new(track.width(), thumb_len),
            );
            p.rect_filled(track, 4.0, Color32::from_black_alpha(40));
            p.rect_filled(thumb, 4.0, if resp.hovered() || resp.dragged() { hover_col } else { idle_col });
            if resp.dragged() {
                // Thumb down = view moves down = pan.y decreases (origin up).
                self.pan.y -= resp.drag_delta().y * (content.y / vp.y);
                self.zoom_anim = None;
            }
        }

        if h_show {
            let track = Rect::from_min_max(
                Pos2::new(avail.min.x + 2.0, avail.max.y - 12.0),
                Pos2::new(avail.max.x - if v_show { 12.0 } else { 4.0 }, avail.max.y - 4.0),
            );
            let resp = ui.interact(track, egui::Id::unique("lumo-hscroll"), egui::Sense::drag());
            let frac = (vp.x / content.x).clamp(0.05, 1.0);
            let thumb_len = track.width() * frac;
            let scrollable = content.x - vp.x;
            let t = (-self.pan.x / scrollable).clamp(0.0, 1.0);
            let thumb = Rect::from_min_size(
                Pos2::new(track.min.x + t * (track.width() - thumb_len), track.min.y),
                Vec2::new(thumb_len, track.height()),
            );
            p.rect_filled(track, 4.0, Color32::from_black_alpha(40));
            p.rect_filled(thumb, 4.0, if resp.hovered() || resp.dragged() { hover_col } else { idle_col });
            if resp.dragged() {
                self.pan.x -= resp.drag_delta().x * (content.x / vp.x);
                self.zoom_anim = None;
            }
        }
    }

    fn edge_arrows(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, avail: Rect) {
        let zone_w = 88.0f32.min(avail.width() * 0.4);
        if zone_w < 8.0 {
            return;
        }
        let left_zone = Rect::from_min_size(avail.min, Vec2::new(zone_w, avail.height()));
        let right_zone = Rect::from_min_max(
            Pos2::new(avail.max.x - zone_w, avail.min.y),
            avail.max,
        );

        let has_prev = self.selection_has_prev();
        let has_next = self.selection_has_next();
        if !has_prev && !has_next {
            return;
        }

        let pointer = ctx.input(|i| i.pointer.hover_pos());

        // Interaction first (on top of the pan/crop area), then painting.
        if has_prev {
            let resp = ui.interact(left_zone, egui::Id::unique("lumo-edge-left"), egui::Sense::click());
            if resp.clicked() {
                self.do_action(ctx, Action::Prev);
            }
        }
        if has_next {
            let resp = ui.interact(right_zone, egui::Id::unique("lumo-edge-right"), egui::Sense::click());
            if resp.clicked() {
                self.do_action(ctx, Action::Next);
            }
        }

        let Some(pp) = pointer else { return };

        // Chevron geometry: three joints of an angular ">" arrow.
        let size = zone_w * 0.62;
        const CHEVRON: &[(f32, f32)] = &[(-0.34, -0.40), (0.34, 0.0), (-0.34, 0.40)];

        let mid_y = avail.center().y;

        for (left, zone, has) in [
            (true, left_zone, has_prev),
            (false, right_zone, has_next),
        ] {
            if !has {
                continue;
            }
            // Animate to 1 while hovered and back to 0 when the cursor
            // leaves — painting continues during the fade-out, which keeps
            // the strip from snapping or leaving artifacts behind.
            let hovered = zone.contains(pp);
            let t = ctx.animate_value_with_time(
                egui::Id::unique(if left { "lumo-edge-left-fade" } else { "lumo-edge-right-fade" }),
                if hovered { 1.0 } else { 0.0 },
                0.15,
            );
            if t <= 0.01 {
                continue;
            }
            let alpha = ((t * 255.0).round() as u8).clamp(0, 255);
            let dir = if left { -1.0 } else { 1.0 };
            let cx = zone.center().x + dir * (1.0 - t) * 12.0; // slide from the edge
            let cyc = mid_y;

            // Soft gradient: dark at the window edge, fading to transparent
            // toward the image. Built from thin per-column strips instead of
            // one big quad — a two-triangle gradient showed hard diagonal
            // seams on some Windows GL drivers, while per-column strips
            // render identically everywhere.
            let mut mesh = egui::Mesh::default();
            let bands = ((zone_w * 0.6).ceil() as usize).max(8);
            let step = zone_w / bands as f32;
            let max_a = ((70.0f32 * t).round()).clamp(0.0, 90.0);
            let mut prev_col: Option<u32> = None;
            for i in 0..=bands {
                let a = (max_a * (1.0 - i as f32 / bands as f32)).round() as u8;
                let c = Color32::from_black_alpha(a);
                let x = if left {
                    zone.min.x + i as f32 * step
                } else {
                    zone.max.x - i as f32 * step
                };
                let v = mesh.vertices.len() as u32;
                mesh.colored_vertex(Pos2::new(x, zone.min.y), c);
                mesh.colored_vertex(Pos2::new(x, zone.max.y), c);
                if let Some(p) = prev_col {
                    mesh.add_triangle(p, p + 1, v);
                    mesh.add_triangle(v, p + 1, v + 1);
                }
                prev_col = Some(v);
            }
            ui.painter().add(egui::Shape::mesh(mesh));

            // Angular chevron (flaticon right-arrow style) with round joints,
            // in a light gray so it doesn't glare on bright images.
            let gray = Color32::from_rgba_unmultiplied(205, 205, 205, 255);
            let stroke_w = size * 0.15;
            let stroke = egui::Stroke::new(
                stroke_w,
                Color32::from_rgba_unmultiplied(gray.r(), gray.g(), gray.b(), alpha),
            );
            let pts: Vec<Pos2> = CHEVRON
                .iter()
                .map(|(dx, dy)| Pos2::new(cx + dir * dx * size, cyc + dy * size))
                .collect();
            ui.painter().add(egui::Shape::line(pts.clone(), stroke));
            for p in pts {
                ui.painter().add(egui::Shape::circle_filled(p, stroke_w * 0.5, stroke.color));
            }
        }
    }

    /// Is there a page before the current one anywhere in the selection?
    fn selection_has_prev(&self) -> bool {
        self.cur_doc > 0 || self.cur_page > 0
    }

    /// Is there a page after the current one anywhere in the selection?
    fn selection_has_next(&self) -> bool {
        let more_pages = self.cur_page + 1 < self.pages();
        let more_docs = self.cur_doc + 1 < self.docs.len();
        more_pages || more_docs
    }

    /// Wheel zoom: one ratio per wheel distance traveled, identical for mouse
    /// notches (Line unit) and trackpad pixels (Point unit). Percent per notch
    /// is user-configurable. Anchored at the cursor and animated smoothly.
    fn zoom_wheel(&mut self, ctx: &egui::Context, wheel_lines: f32, pointer: Pos2) {
        let avail = self.last_avail;
        if avail.is_negative() {
            return;
        }
        let fit = self.fit_scale(avail.size(), ctx);
        let old_scale = match self.zoom {
            Zoom::Fit => {
                if wheel_lines <= 0.0 {
                    return; // can't zoom out from fit
                }
                fit
            }
            Zoom::Scale(s) => s,
        };
        // positive wheel = zoom in; `wheel_zoom_pct`% per notch.
        let pct = self.wheel_zoom_pct.clamp(1.0, 80.0);
        let factor = (wheel_lines * (1.0 + pct / 100.0).ln()).exp().clamp(0.80, 1.30);
        let new = (old_scale * factor).clamp(0.02, 40.0);

        // Anchor point is always computed from the *target* values below.
        let origin = self.view_origin(avail, ctx);
        let delta = pointer - origin;

        // animate from wherever we are displayed right now
        self.zoom_anim_start(fit);

        if new <= fit * 1.02 {
            self.zoom = Zoom::Fit;
            self.pan = Vec2::ZERO;
            return;
        }
        self.zoom = Zoom::Scale(new);
        // Keep the point under the cursor stable in image space (target pan).
        let ratio = new / old_scale;
        self.pan += delta - delta * ratio;
    }

    fn crop_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: Rect,
        resp: &egui::Response,
        img_size: Vec2,
    ) {
        let sx = rect.width() / img_size.x.max(1.0);
        let sy = rect.height() / img_size.y.max(1.0);
        let to_img = |p: Pos2| Pos2::new((p.x - rect.min.x) / sx, (p.y - rect.min.y) / sy);

        if resp.drag_started() {
            if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                let p_img = to_img(p);
                if rect.contains(p) {
                    self.crop_start = Some(p_img);
                    self.crop_rect = None;
                }
            }
        }
        if let Some(start) = self.crop_start {
            if resp.dragged() {
                if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                    let p_img = to_img(p);
                    self.crop_rect = Some(Rect::from_two_pos(start, p_img));
                }
            }
            if resp.drag_stopped() {
                if let Some(r) = self.crop_rect {
                    if r.width() < 4.0 || r.height() < 4.0 {
                        // too small to be a real selection: discard it
                        self.crop_rect = None;
                    }
                }
                // The box is finalized either way; the next drag starts a
                // fresh one and the apply/cancel bar may now appear.
                self.crop_start = None;
            }
        }

        // Overlay: dim everything outside the crop box, then park the
        // apply/cancel bar just under the selection (flipped above it when
        // there isn't room below).
        if let Some(cr) = self.crop_rect {
            let screen = Rect::from_min_size(
                rect.min + Vec2::new(cr.min.x * sx, cr.min.y * sy),
                Vec2::new(cr.width() * sx, cr.height() * sy),
            );
            let p = ui.painter();
            // Dim only what is outside the box: four strips around it.
            let left = Rect::from_min_max(Pos2::new(rect.min.x, rect.min.y), Pos2::new(screen.min.x, rect.max.y));
            let right = Rect::from_min_max(Pos2::new(screen.max.x, rect.min.y), Pos2::new(rect.max.x, rect.max.y));
            let top = Rect::from_min_max(Pos2::new(screen.min.x, rect.min.y), Pos2::new(screen.max.x, screen.min.y));
            let bottom = Rect::from_min_max(Pos2::new(screen.min.x, screen.max.y), Pos2::new(screen.max.x, rect.max.y));
            for r in [left, right, top, bottom] {
                if r.width() > 0.0 && r.height() > 0.0 {
                    p.rect_filled(r, 0.0, Color32::from_black_alpha(140));
                }
            }
            p.rect_stroke(screen, 0.0, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Middle);

            // The bar only shows once the box is finalized (drag released).
            if self.crop_start.is_none() {
                let bar_w = 252.0;
                let bar_h = 40.0;
                let mut origin = Pos2::new(screen.center().x - bar_w / 2.0, screen.max.y + 10.0);
                if origin.y + bar_h > rect.max.y {
                    origin.y = (screen.min.y - bar_h - 10.0).max(rect.min.y);
                }
                origin.x = origin
                    .x
                    .clamp(rect.min.x, (rect.max.x - bar_w).max(rect.min.x));

                Area::new(egui::Id::unique("lumo-crop"))
                    .fixed_pos(origin)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.style_mut().spacing.button_padding = Vec2::new(14.0, 9.0);
                            if ui
                                .add_sized(
                                    [132.0, 36.0],
                                    egui::Button::new("Apply crop (⏎)"),
                                )
                                .on_hover_text("Crop to the highlighted area")
                                .clicked()
                            {
                                self.do_action(ctx, Action::CropApply);
                            }
                            if ui
                                .add_sized([104.0, 36.0], egui::Button::new("Cancel (Esc)"))
                                .clicked()
                            {
                                self.do_action(ctx, Action::CropCancel);
                            }
                        });
                    });
            }
        }
    }
}

/// Natural-ish compare: runs of digits compare by value ("img2" < "img10").
fn nat_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ai, mut bi) = (0, 0);
    let bytes = |s: &str, i: usize| s.as_bytes().get(i).copied().unwrap_or(0);
    loop {
        let ca = bytes(a, ai);
        let cb = bytes(b, bi);
        if ca == 0 && cb == 0 {
            return std::cmp::Ordering::Equal;
        }
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // take the whole digit run on both sides
            let a0 = ai;
            let b0 = bi;
            while bytes(a, ai).is_ascii_digit() {
                ai += 1;
            }
            while bytes(b, bi).is_ascii_digit() {
                bi += 1;
            }
            let na: u64 = a[a0..ai].parse().unwrap_or(u64::MAX);
            let nb: u64 = b[b0..bi].parse().unwrap_or(u64::MAX);
            if na != nb {
                return na.cmp(&nb);
            }
        } else {
            let la = a[ai..].chars().next().unwrap_or('\0');
            let lb = b[bi..].chars().next().unwrap_or('\0');
            let fla = la.to_ascii_lowercase();
            let flb = lb.to_ascii_lowercase();
            match fla.cmp(&flb) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
            ai += la.len_utf8().max(1);
            bi += lb.len_utf8().max(1);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Next,
    Prev,
    NextDoc,
    PrevDoc,
    First,
    Last,
    ZoomIn,
    ZoomOut,
    Fit,
    Zoom100,
    Open,
    Save,
    SaveAs,
    Copy,
    Undo,
    RotateL,
    RotateR,
    FlipH,
    FlipV,
    Grayscale,
    Sharpen,
    Crop,
    CropApply,
    CropCancel,
    Panel,
    Fullscreen,
    Slideshow,
    Exit,
}

impl Lumo {
    /// Capture the window rect and save settings + session once on close.
    fn save_on_exit(&mut self, ctx: &egui::Context) {
        if self.exit_saved {
            return;
        }
        self.exit_saved = true;
        if self.remember_win && !self.fullscreen {
            let vp = ctx.input(|i| i.viewport().clone());
            let rect = vp.inner_rect.or(vp.outer_rect);
            if let Some(r) = rect {
                if r.width() > 100.0 && r.height() > 100.0 {
                    self.win_rect = Some(r);
                }
            }
        }
        self.save_settings();
        if self.reopen_last {
            self.save_last_session();
        }
    }

    /// Slideshow: advance on the timer, wrap or stop at the end.
    fn slideshow_tick(&mut self, ctx: &egui::Context) {
        if !self.sliding || self.docs.is_empty() {
            return;
        }
        let now = Instant::now();
        let secs = self.slide_secs.clamp(0.5, 120.0);
        let due = self
            .slide_next
            .unwrap_or(now + std::time::Duration::from_secs_f32(secs));
        if now >= due {
            self.slide_next = Some(now + std::time::Duration::from_secs_f32(secs));
            let last_doc = self.cur_doc + 1 >= self.docs.len();
            let last_page = self.cur_page + 1 >= self.pages();
            if last_doc && last_page && !self.slide_loop {
                self.sliding = false;
                self.slide_next = None;
                self.flash = Some(("Slideshow ended".into(), Instant::now()));
            } else {
                self.next_page();
            }
        }
        // egui sleeps while idle — wake it for the next slide.
        if let Some(t) = self.slide_next {
            let wait = (t - Instant::now()).as_secs_f32();
            if wait > 0.0 {
                ctx.request_repaint_after(std::time::Duration::from_secs_f32(wait + 0.01));
            }
        }
    }
}

impl eframe::App for Lumo {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // One-time startup: theme, remembered window, launch state.
        if !self.startup_done {
            self.startup_done = true;
            ctx.set_theme(if self.theme_light {
                egui::ThemePreference::Light
            } else {
                egui::ThemePreference::Dark
            });
            if self.start_fs && !self.fullscreen {
                self.do_action(ctx, Action::Fullscreen);
            }
        }
        // Persist window geometry and the open files when closing.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save_on_exit(ctx);
        }

        self.poll_worker();
        self.ensure_current();
        self.prefetch();
        self.trim_cache();
        self.clamp_to_monitor(ctx);
        self.slideshow_tick(ctx);

        // expire old toasts while hidden
        if let Some((_, t)) = self.toast.clone() {
            if t.elapsed().as_secs_f32() >= 6.0 {
                self.toast = None;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.incoming_files(&ctx);
        self.handle_input(&ctx);

        // In fullscreen the image gets the whole window: the top bar and
        // status line slide away, and reappear whenever the mouse moves.
        if self.fullscreen {
            let pos = ctx.input(|i| i.pointer.latest_pos());
            if pos != self.last_pointer {
                self.last_pointer = pos;
                self.chrome_until = Some(
                    std::time::Instant::now()
                        + std::time::Duration::from_secs_f32(self.chrome_hide_secs.max(0.0)),
                );
                // cursor stays visible while the bars are up
                if self.hide_cursor_secs > 0.0 {
                    self.cursor_until = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_secs_f32(self.hide_cursor_secs),
                    );
                }
            }
            // egui sleeps while idle; without this the expiry instant would
            // never be painted and the bars would stay on screen.
            if let Some(t) = self.chrome_until {
                let wait = (t - std::time::Instant::now()).as_secs_f32();
                if wait > 0.0 {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(wait + 0.02));
                }
            }
            if self.hide_cursor_secs > 0.0 {
                let cursor_due = self
                    .cursor_until
                    .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs_f32(self.hide_cursor_secs));
                let wait = (cursor_due - std::time::Instant::now()).as_secs_f32();
                if wait > 0.0 {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f32(wait + 0.02));
                }
            }
        } else {
            self.chrome_until = None;
            self.cursor_until = None;
        }
        let show_chrome = !self.fullscreen
            || self
                .chrome_until
                .is_some_and(|t| std::time::Instant::now() < t);
        // Hide the mouse cursor after it has been idle in fullscreen a while.
        if self.fullscreen
            && self.hide_cursor_secs > 0.0
            && !show_chrome
            && self
                .cursor_until
                .is_some_and(|t| std::time::Instant::now() >= t)
        {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }

        // Drive the slide animation: full travel in 0.3 s.
        let target: f32 = if show_chrome { 1.0 } else { 0.0 };
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.05);
        let step = dt / 0.30;
        self.chrome_anim = if self.chrome_anim < target {
            (self.chrome_anim + step).min(target)
        } else {
            (self.chrome_anim - step).max(target)
        };
        if self.fullscreen && self.chrome_anim != target {
            ctx.request_repaint(); // keep the animation running
        }

        // Flush stale frames after a window resize (fullscreen toggles).
        if let Some(t) = self.repaint_until {
            if std::time::Instant::now() < t {
                ctx.request_repaint();
            } else {
                self.repaint_until = None;
            }
        }

        self.topbar(ui);
        self.edit_panel(ui);
        self.status(ui);

        CentralPanel::default()
            .frame(Frame::NONE)
            .show(ui, |ui| self.view_ui(ui));

        if let Some((msg, _)) = self.toast.clone() {
            Area::new(egui::Id::unique("lumo-toast"))
                .anchor(Align2::LEFT_BOTTOM, [12.0, -12.0])
                .show(&ctx, |ui| {
                    ui.label(
                        egui::RichText::new(msg)
                            .color(Color32::LIGHT_RED)
                            .size(14.0),
                    );
                });
        }

        // repaint while decode/edit work races us
        if self.pending.contains(&self.key()) || !self.adjust.is_neutral() {
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nat_cmp;

    #[test]
    fn natural_order() {
        let mut names = vec!["img10.png", "img2.png", "Img1.png", "img20.png", "img3.png"];
        names.sort_by(|a, b| nat_cmp(a, b));
        assert_eq!(names, vec!["Img1.png", "img2.png", "img3.png", "img10.png", "img20.png"]);
    }

    #[test]
    fn natural_order_reversed() {
        let mut names = vec!["page 2", "page 10", "page 1"];
        names.sort_by(|a, b| nat_cmp(a, b));
        names.reverse();
        assert_eq!(names, vec!["page 10", "page 2", "page 1"]);
    }
}
