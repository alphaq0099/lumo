//! The egui application: single window, selection-bounded navigation,
//! zoom/pan, crop/rotate/flip/adjust editing, saving.

use crate::decode::{self, PageKey};
use crate::doc::{self, Document};
use crate::edit::{self, EditOp};
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
    approx_bytes: usize,
}

impl Entry {
    fn empty() -> Self {
        Self {
            original: None,
            failed: None,
            edited: None,
            texture: None,
            tex_sig: None,
            approx_bytes: 0,
        }
    }
}

const MAX_CACHE_BYTES: usize = 512 * 1024 * 1024;

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
    last_avail: Rect,
    fullscreen: bool,

    panel_open: bool,
    crop_mode: bool,
    crop_start: Option<Pos2>, // drag start in image coords
    crop_rect: Option<Rect>,  // finalized, image coords

    toast: Option<(String, Instant)>,
    flash: Option<(String, Instant)>,
}

enum Prepared {
    Loading,
    Failed(String),
    Ready {
        texture: Tex,
        #[allow(dead_code)] // future: overlays while the image is under adjustment
        edited: bool,
    },
}

impl Lumo {
    pub fn new(docs: Vec<Document>) -> Self {
        let (worker_tx, worker_rx) = decode::spawn();
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
            wheel_zoom_pct: Self::load_wheel_zoom_pct(),
            last_avail: Rect::from_min_size(Pos2::ZERO, Vec2::splat(600.0)),
            fullscreen: false,
            panel_open: false,
            crop_mode: false,
            crop_start: None,
            crop_rect: None,
            toast: None,
            flash: None,
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

    fn goto(&mut self, doc: usize, page: usize) {
        self.cur_doc = doc.min(self.docs.len().saturating_sub(1));
        let pmax = self.docs.get(self.cur_doc).map(|d| d.pages.len()).unwrap_or(1);
        self.cur_page = if pmax == 0 { 0 } else { page.min(pmax - 1) };
        self.reset_view();
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

    fn load_wheel_zoom_pct() -> f32 {
        let Some(path) = Self::config_path() else { return 13.0 };
        match std::fs::read_to_string(&path) {
            Ok(txt) => {
                for line in txt.lines() {
                    if let Some(v) = line.strip_prefix("wheel_zoom_pct") {
                        let v = v.trim().trim_start_matches('=').trim();
                        if let Ok(p) = v.parse::<f32>() {
                            return p.clamp(1.0, 80.0);
                        }
                    }
                }
                13.0
            }
            Err(_) => 13.0,
        }
    }

    fn save_settings(&self) {
        let Some(path) = Self::config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let txt = format!("wheel_zoom_pct = {:.1}\n", self.wheel_zoom_pct.round());
        let _ = std::fs::write(&path, txt);
    }

    fn next_page(&mut self) {
        if self.cur_page + 1 < self.pages() {
            self.cur_page += 1;
            self.reset_view();
        } else if self.cur_doc + 1 < self.docs.len() {
            self.goto(self.cur_doc + 1, 0);
        } else if !self.docs.is_empty() {
            self.goto(0, 0);
        }
    }

    fn prev_page(&mut self) {
        if self.cur_page > 0 {
            self.cur_page -= 1;
            self.reset_view();
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
            for dp in [curp + 1, curp + 2, curp.wrapping_sub(1)] {
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
            let entry = match res {
                Ok(decode::DecodedImage { image }) => Entry {
                    original: Some(Arc::new(image)),
                    approx_bytes: 0, // filled below
                    ..Entry::empty()
                },
                Err(e) => Entry {
                    failed: Some(format!("{e}")),
                    ..Entry::empty()
                },
            };
            let mut entry = entry;
            if let Some(o) = &entry.original {
                entry.approx_bytes =
                    (o.width() as usize) * (o.height() as usize) * 8 + 64;
            }
            let was_new = self.cache.insert(key, entry).is_none();
            if was_new {
                self.cache_order.push(key);
                self.cache_bytes += self.cache.get(&key).map(|e| e.approx_bytes).unwrap_or(0);
                self.evict();
            }
        }
    }

    fn evict(&mut self) {
        let cur = self.key();
        while self.cache_bytes > MAX_CACHE_BYTES && self.cache_order.len() > 3 {
            let Some(idx) = self.cache_order.iter().position(|k| *k != cur) else {
                break;
            };
            let k = self.cache_order.remove(idx);
            if let Some(e) = self.cache.remove(&k) {
                self.cache_bytes = self.cache_bytes.saturating_sub(e.approx_bytes);
            }
        }
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
            match crate::save::save_as(&image, &path) {
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
        match crate::save::overwrite(&image, &path, has_edits) {
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
            Action::Zoom100 => {
                self.zoom = Zoom::Scale(1.0);
                self.pan = Vec2::ZERO;
            }
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
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
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
        (avail.x / tex.x).min(avail.y / tex.y)
    }

    fn best_texture_size(&self, _ctx: &egui::Context) -> Option<Vec2> {
        let key = self.key();
        let e = self.cache.get(&key)?;
        let img = e.edited.as_ref().map(|(_, i)| i).or(e.original.as_ref())?;
        Some(Vec2::new(img.width() as f32, img.height() as f32))
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
        if size.x <= avail.width() && size.y <= avail.height() {
            avail.min + (avail.size() - size) / 2.0
        } else {
            avail.min + self.pan
        }
    }

    // ------------------------------------------------------- prepared image

    fn prepare(&mut self, ctx: &egui::Context) -> Prepared {
        let key = self.key();
        if !self.cache.contains_key(&key) {
            self.ensure_current();
            return Prepared::Loading;
        }
        let entry = self.cache.get(&key).unwrap();
        if let Some(f) = entry.failed.clone() {
            return Prepared::Failed(f);
        }
        let Some(original) = entry.original.as_ref().map(Arc::clone) else {
            return Prepared::Loading;
        };

        let ops = self.effective_ops(key);
        let sig = edit::signature(&ops);
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
        if entry.tex_sig.as_deref() != Some(sig.as_str())
            || entry.texture.is_none()
        {
            let rgba = image.to_rgba8();
            let col = ColorImage::from_rgba_unmultiplied(
                [rgba.width() as usize, rgba.height() as usize],
                rgba.into_raw().as_slice(),
            );
            let tex = ctx.load_texture("lumo-page", col, egui::TextureOptions::LINEAR);
            entry.texture = Some(tex);
            entry.tex_sig = Some(sig);
        }

        Prepared::Ready {
            texture: entry.texture.clone().unwrap(),
            edited: !ops.is_empty(),
        }
    }

    // ------------------------------------------------------- panels

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        Panel::top("lumo-toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let prev = ui.add(egui::Button::new("◀ prev"));
                if prev.clicked() {
                    self.do_action(ui.ctx(), Action::Prev);
                }
                prev.on_hover_text("Previous page (←)");

                let next = ui.add(egui::Button::new("next ▶"));
                if next.clicked() {
                    self.do_action(ui.ctx(), Action::Next);
                }
                next.on_hover_text("Next page (→ / Space)");

                ui.separator();
                if ui.add(egui::Button::new("(prev file)")).clicked() {
                    self.do_action(ui.ctx(), Action::PrevDoc);
                }
                if ui.add(egui::Button::new("(next file)")).clicked() {
                    self.do_action(ui.ctx(), Action::NextDoc);
                }
                ui.separator();
                if ui.add(egui::Button::new("Fit (0)")).clicked() {
                    self.do_action(ui.ctx(), Action::Fit);
                }
                if ui.add(egui::Button::new("100% (1)")).clicked() {
                    self.do_action(ui.ctx(), Action::Zoom100);
                }
                ui.separator();
                ui.label(egui::RichText::new("wheel").weak().size(11.0));
                let zr = ui.add(
                    egui::DragValue::new(&mut self.wheel_zoom_pct)
                        .speed(0.5)
                        .range(1.0..=80.0)
                        .suffix("%"),
                )
                .on_hover_text(
                    "How much one scroll-wheel notch zooms in/out. \
                     Click and drag sideways to change.",
                );
                if zr.changed() {
                    self.save_settings();
                }
                ui.separator();
                if ui.add(egui::Button::new("Open…")).clicked() {
                    self.do_action(ui.ctx(), Action::Open);
                }
                ui.separator();
                if ui.add(egui::Button::new("↺")).clicked() {
                    self.do_action(ui.ctx(), Action::RotateL);
                }
                if ui.add(egui::Button::new("↻")).clicked() {
                    self.do_action(ui.ctx(), Action::RotateR);
                }
                if ui.add(egui::Button::new("⇋ H")).clicked() {
                    self.do_action(ui.ctx(), Action::FlipH);
                }
                if ui.add(egui::Button::new("⇅ V")).clicked() {
                    self.do_action(ui.ctx(), Action::FlipV);
                }
                if self.crop_mode {
                    if ui.add(egui::Button::new("✓ crop")).clicked() {
                        self.do_action(ui.ctx(), Action::CropApply);
                    }
                    if ui.add(egui::Button::new("✕ cancel")).clicked() {
                        self.do_action(ui.ctx(), Action::CropCancel);
                    }
                } else if ui.add(egui::Button::new("Rect")).clicked() {
                    self.do_action(ui.ctx(), Action::Crop);
                }
                if ui.add(egui::Button::new("B/W")).clicked() {
                    self.do_action(ui.ctx(), Action::Grayscale);
                }
                if ui.add(egui::Button::new("Adjust…")).clicked() {
                    self.do_action(ui.ctx(), Action::Panel);
                }
                ui.separator();
                if ui.add(egui::Button::new("Undo (Ctrl+Z)")).clicked() {
                    self.do_action(ui.ctx(), Action::Undo);
                }
                if ui.add(egui::Button::new("Save (Ctrl+S)")).clicked() {
                    self.do_action(ui.ctx(), Action::Save);
                }
                if ui.add(egui::Button::new("Save As…")).clicked() {
                    self.do_action(ui.ctx(), Action::SaveAs);
                }
                if ui.add(egui::Button::new("Copy (Ctrl+C)")).clicked() {
                    self.do_action(ui.ctx(), Action::Copy);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new("🗖")).clicked() {
                        self_do_fullscreen(self, ui.ctx());
                    }
                });
            });
            ui.add_space(4.0);
        });
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
        Panel::bottom("lumo-status").show(ui, |ui| {
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
                ui.label(
                    egui::RichText::new(location)
                        .weak()
                        .family(egui::FontFamily::Proportional),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // current zoom, right before the transient messages
                    let fit = self.fit_scale(self.last_avail.size(), ui.ctx());
                    let (scale, _) = self.zoom_display(fit);
                    let ztxt = match self.zoom {
                        Zoom::Fit => "fit".to_string(),
                        Zoom::Scale(_) => format!("{:.0}%", (scale * 100.0).round()),
                    };
                    ui.label(
                        egui::RichText::new(ztxt)
                            .weak()
                            .monospace(),
                    )
                    .on_hover_text(format!(
                        "current zoom · each wheel notch steps {:.0}%",
                        self.wheel_zoom_pct.clamp(1.0, 80.0).round()
                    ));
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
                    ui.label(
                        egui::RichText::new(format!(
                            "{zoom_label}{}",
                            if self.edits.contains_key(&self.key()) {
                                format!(
                                    " · {} edits",
                                    self.edits.get(&self.key()).map(|v| v.len()).unwrap_or(0)
                                )
                            } else {
                                String::new()
                            }
                        ))
                        .weak(),
                    );
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
        });
    }

    // ------------------------------------------------------- image view

    fn view_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        self.last_avail = avail;
        let resp = ui.allocate_rect(avail, egui::Sense::click_and_drag());
        let ctx = ui.ctx().clone();

        let prepared = self.prepare(&ctx);
        let mut repainted_hints = false;
        let _ = &mut repainted_hints;

        match prepared {
            Prepared::Loading => {
                ui.put(avail.shrink2(Vec2::splat(80.0)), egui::Spinner::new().size(56.0));
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

                // Clamp pan so the image doesn't wander off.
                let size = Vec2::new(img_size.x * scale, img_size.y * scale);
                let clamp_axis = |p: f32, avail_v: f32, size_v: f32| -> f32 {
                    if size_v <= avail_v {
                        0.0
                    } else {
                        p.clamp(avail_v - size_v, 0.0)
                    }
                };
                pan.x = clamp_axis(pan.x, avail.width(), size.x);
                pan.y = clamp_axis(pan.y, avail.height(), size.y);
                if self.zoom_anim.is_none() {
                    self.pan = pan;
                }
                let origin = if size.x <= avail.width() && size.y <= avail.height() {
                    avail.min + (avail.size() - size) / 2.0
                } else {
                    avail.min + pan
                };
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
                    if wheel_lines.abs() > 0.01 {
                        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                            self.zoom_wheel(&ctx, wheel_lines, p);
                        }
                    }
                }

                // ---- drag pan
                if resp.dragged() && !self.crop_mode {
                    self.pan -= resp.drag_delta();
                    self.zoom_anim = None;
                }

                // ---- double click: fit ↔ 100%
                if resp.double_clicked() {
                    self.zoom = if self.zoom == Zoom::Scale(1.0) {
                        Zoom::Fit
                    } else {
                        Zoom::Scale(1.0)
                    };
                    self.pan = Vec2::ZERO;
                    self.zoom_anim = None;
                }

                // ---- edge hover arrows (appear at the window ends only
                //      when there is a previous/next page and the cursor
                //      is near that end)
                if !self.crop_mode {
                    self.edge_arrows(ui, &ctx, avail);
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
                        self.crop_rect = None;
                        self.crop_start = None;
                    }
                } else {
                    self.crop_start = None;
                }
            }
        }

        // Overlay: dim everything outside the crop box.
        if let Some(cr) = self.crop_rect {
            let screen = Rect::from_min_size(
                rect.min + Vec2::new(cr.min.x * sx, cr.min.y * sy),
                Vec2::new(cr.width() * sx, cr.height() * sy),
            );
            let p = ui.painter();
            p.rect_filled(rect, 0.0, Color32::from_black_alpha(140));
            p.rect_filled(screen, 0.0, Color32::TRANSPARENT);
            // punch the hole by drawing the four dimmed strips
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
        }

        // Floating apply/cancel bar
        Area::new(egui::Id::unique("lumo-crop"))
            .anchor(Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button("Apply crop (⏎)")
                        .on_hover_text("Crop to the highlighted area")
                        .clicked()
                    {
                        self.do_action(ctx, Action::CropApply);
                    }
                    if ui.button("Cancel (Esc)").clicked() {
                        self.do_action(ctx, Action::CropCancel);
                    }
                });
            });
    }
}

fn self_do_fullscreen(app: &mut Lumo, ctx: &egui::Context) {
    app.do_action(ctx, Action::Fullscreen);
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
}

impl eframe::App for Lumo {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
        self.ensure_current();
        self.prefetch();

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

        self.toolbar(ui);
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
