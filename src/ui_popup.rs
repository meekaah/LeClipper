//! Post-recording popup (runs as `screen2imgur.exe popup <file> <w> <h> <ms>`).
//!
//! Top to bottom: a header with the clip's details, the looping preview (click to pause,
//! drag to drop the file anywhere), the action bar, and a scrolling stack of running jobs
//! and results, with the latest message pinned under it.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Shadow,
    Stroke, StrokeKind, Ui,
};

use crate::config::{Config, DefaultAction, DragFormat};
use crate::gif::{GifOptions, Progress};
use crate::ui_settings::{centered_galley_pos, icons, mix, semibold};

pub struct Args {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

// Palette.
const CANVAS: Color32 = Color32::from_rgb(18, 19, 24);
const CARD: Color32 = Color32::from_rgb(26, 28, 35);
const HOVER: Color32 = Color32::from_rgb(35, 38, 49);
const BORDER: Color32 = Color32::from_rgb(42, 45, 58);
const FIELD: Color32 = Color32::from_rgb(14, 15, 19);
const VIDEO_BG: Color32 = Color32::from_rgb(11, 12, 15);
const WHITE: Color32 = Color32::WHITE;
const TEXT: Color32 = Color32::from_rgb(225, 228, 234);
const WEAK: Color32 = Color32::from_rgb(140, 146, 164);
const ACCENT: Color32 = Color32::from_rgb(27, 183, 110);
const ACCENT_HOVER: Color32 = Color32::from_rgb(20, 155, 92);
const SURFACE: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
const SURFACE_BORDER: Color32 = Color32::from_rgba_premultiplied(20, 20, 20, 20);
const SUCCESS: Color32 = Color32::from_rgb(52, 211, 153);
const SUCCESS_FILL: Color32 = Color32::from_rgb(18, 38, 33);
const ERR_TEXT: Color32 = Color32::from_rgb(248, 113, 113);
const ERR_FILL: Color32 = Color32::from_rgb(69, 10, 10);
const ERR_BORDER: Color32 = Color32::from_rgb(153, 27, 27);
const DANGER: Color32 = Color32::from_rgb(220, 38, 38);
const DANGER_HOVER: Color32 = Color32::from_rgb(185, 28, 28);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
const AMBER_FILL: Color32 = Color32::from_rgb(51, 37, 12);
const AMBER_BORDER: Color32 = Color32::from_rgb(120, 84, 18);
const INFO: Color32 = Color32::from_rgb(96, 165, 250);
const INFO_FILL: Color32 = Color32::from_rgb(23, 30, 46);
const INFO_BORDER: Color32 = Color32::from_rgb(38, 52, 82);

// Segoe Fluent Icons / Segoe MDL2 Assets code points.
const ICON_UPLOAD: &str = "\u{E898}";
const ICON_SAVE: &str = "\u{E74E}";
const ICON_VIDEO: &str = "\u{E714}";
const ICON_PICTURE: &str = "\u{E8B9}";
const ICON_CLOSE: &str = "\u{E711}";
const ICON_PLAY: &str = "\u{E768}";
const ICON_CHEVRON: &str = "\u{E70D}";
const ICON_COPY: &str = "\u{E8C8}";
const ICON_OPEN: &str = "\u{E8A7}";
const ICON_WARNING: &str = "\u{E7BA}";
const ICON_CHECK: &str = "\u{E73E}";
const ICON_ERROR: &str = "\u{E783}";
const ICON_INFO: &str = "\u{E946}";

/// How long the Copy button says "Copied" after a copy.
const COPIED_TIME: Duration = Duration::from_secs(2);
/// The preview never gets shorter than this to make room below it.
const MIN_PREVIEW_H: f32 = 140.0;
const WINDOW_W: f32 = 660.0;
/// The panel's horizontal and vertical inner margins.
const MARGIN: (i8, i8) = (18, 14);

pub fn run(args: Args) -> Result<()> {
    if !args.path.exists() {
        return Err(anyhow!("{} does not exist", args.path.display()));
    }
    let temp_mp4 = args.path.clone();
    let temp_gif = temp_mp4.with_extension("gif");
    // Close to what the window fits itself to on the first frame, so it doesn't visibly jump.
    let preview_w = WINDOW_W - 2.0 * MARGIN.0 as f32;
    let preview_h = (preview_w * args.height.max(1) as f32 / args.width.max(1) as f32).min(args.height as f32);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screen2imgur — recording")
            .with_inner_size([WINDOW_W, preview_h.max(MIN_PREVIEW_H) + 147.0])
            .with_min_inner_size([440.0, 260.0])
            .with_always_on_top()
            .with_active(true)
            .with_icon(crate::ui_settings::app_icon()),
        ..Default::default()
    };
    let res = eframe::run_native(
        "screen2imgur popup",
        options,
        Box::new(move |cc| Ok(Box::new(Popup::new(cc, args)))),
    )
    .map_err(|e| anyhow!("{e}"));
    let _ = std::fs::remove_file(&temp_mp4);
    let _ = std::fs::remove_file(&temp_gif);
    res
}

#[derive(Default)]
struct JobProgress {
    gif: Arc<Progress>,
}

enum JobDone {
    Saved(PathBuf),
    Failed(String),
}

struct Job {
    progress: Arc<JobProgress>,
    rx: Receiver<JobDone>,
    /// A GIF export (cancellable, reports frames) rather than an MP4 copy.
    gif: bool,
    /// Started from a Save button: the window closes once the file is saved.
    close_when_done: bool,
}

struct Upload {
    job: crate::imgur_web::Job,
    rx: Receiver<crate::imgur_web::Event>,
    status: String,
    progress: Option<f32>,
    /// Lets the tray warn before quitting mid-upload.
    _marker: Option<crate::children::UploadMarker>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Info,
    Error,
}

struct Message {
    text: String,
    tone: Tone,
}

struct Popup {
    args: Args,
    cfg: Config,
    base_name: String,
    preview: crate::preview::Preview,
    drag_format: DragFormat,
    job: Option<Job>,
    saved: Vec<PathBuf>,
    upload: Option<Upload>,
    /// Imgur post link once uploaded.
    link: Option<String>,
    copied_at: Option<Instant>,
    message: Option<Message>,
    confirm_discard: bool,
    ran_default: bool,
    /// Height of everything under the preview last frame, so the preview can leave room for it
    /// when the window can't grow any taller.
    below_h: f32,
    /// The native window, and the content height it was last sized to and centered for,
    /// until the window has settled at that size.
    hwnd: Option<isize>,
    placed_for: Option<f32>,
    placed: bool,
}

impl Popup {
    fn new(cc: &eframe::CreationContext, args: Args) -> Self {
        crate::ui_settings::style_window(cc, CANVAS);
        crate::ui_settings::install_fonts(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let cfg = Config::load();
        let preview = crate::preview::Preview::start(
            &cc.egui_ctx,
            args.path.clone(),
            (args.width, args.height),
            args.duration_ms,
        );
        Self {
            base_name: format!("screen2imgur_{}", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")),
            drag_format: cfg.drag_format,
            cfg,
            args,
            preview,
            job: None,
            saved: Vec::new(),
            upload: None,
            link: None,
            copied_at: None,
            message: None,
            confirm_discard: false,
            ran_default: false,
            below_h: 110.0,
            hwnd: window_handle(cc),
            placed_for: None,
            placed: false,
        }
    }

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.message = Some(Message { text: text.into(), tone });
    }

    fn gif_options(&self) -> GifOptions {
        GifOptions { fps: self.cfg.gif_fps, max_width: self.cfg.gif_max_width, quality: self.cfg.gif_quality }
    }

    fn output_path(&self, ext: &str) -> PathBuf {
        let dir = &self.cfg.output_dir;
        let mut p = dir.join(format!("{}.{ext}", self.base_name));
        let mut n = 2;
        while p.exists() {
            p = dir.join(format!("{}_{n}.{ext}", self.base_name));
            n += 1;
        }
        p
    }

    fn spawn(
        &mut self,
        ctx: &egui::Context,
        gif: bool,
        close_when_done: bool,
        work: impl FnOnce(&JobProgress) -> JobDone + Send + 'static,
    ) {
        let progress = Arc::new(JobProgress::default());
        let (tx, rx) = mpsc::channel();
        let p = progress.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let done = work(&p);
            let _ = tx.send(done);
            ctx.request_repaint();
        });
        self.message = None;
        self.job = Some(Job { progress, rx, gif, close_when_done });
    }

    fn saved_with_ext(&self, ext: &str) -> Option<PathBuf> {
        self.saved.iter().find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))).cloned()
    }

    /// With `close`, the window closes once the file is saved.
    fn save_mp4(&mut self, ctx: &egui::Context, close: bool) {
        if let Some(p) = self.saved_with_ext("mp4") {
            if close {
                self.close_after_save(ctx);
            } else {
                self.say(format!("Already saved as {}", file_name(&p)), Tone::Info);
            }
            return;
        }
        let src = self.args.path.clone();
        let dst = self.output_path("mp4");
        self.spawn(ctx, false, close, move |_| {
            let r = (|| -> Result<()> {
                std::fs::create_dir_all(dst.parent().unwrap_or(Path::new(".")))?;
                std::fs::copy(&src, &dst)?;
                Ok(())
            })();
            match r {
                Ok(()) => JobDone::Saved(dst),
                Err(e) => JobDone::Failed(format!("Saving MP4 failed: {e:#}")),
            }
        });
    }

    /// With `close`, the window closes once the file is saved.
    fn save_gif(&mut self, ctx: &egui::Context, close: bool) {
        if let Some(p) = self.saved_with_ext("gif") {
            if close {
                self.close_after_save(ctx);
            } else {
                self.say(format!("Already saved as {}", file_name(&p)), Tone::Info);
            }
            return;
        }
        let src = self.args.path.clone();
        let dst = self.output_path("gif");
        let crop = (self.args.width, self.args.height);
        let opts = self.gif_options();
        self.spawn(ctx, true, close, move |p| {
            match crate::gif::mp4_to_gif(&src, &dst, crop, &opts, p.gif.clone()) {
                Ok(()) => JobDone::Saved(dst),
                Err(e) => JobDone::Failed(format!("GIF export failed: {e:#}")),
            }
        });
    }

    /// Drags the saved clip out of the window (saving it first, since the temp file
    /// disappears when the popup closes and the browser may still be reading it).
    fn start_drag(&mut self, ctx: &egui::Context) {
        let path = match self.drag_format {
            DragFormat::Mp4 => match self.saved_with_ext("mp4") {
                Some(p) => p,
                None => {
                    let dst = self.output_path("mp4");
                    let copied = std::fs::create_dir_all(dst.parent().unwrap_or(Path::new(".")))
                        .and_then(|_| std::fs::copy(&self.args.path, &dst));
                    if let Err(e) = copied {
                        self.say(format!("Saving MP4 failed: {e}"), Tone::Error);
                        return;
                    }
                    self.saved.push(dst.clone());
                    dst
                }
            },
            DragFormat::Gif => match self.saved_with_ext("gif") {
                Some(p) => p,
                None => {
                    if self.job.is_none() {
                        self.save_gif(ctx, false);
                    }
                    self.say("Making the GIF first. Drag again once it's saved.", Tone::Info);
                    return;
                }
            },
        };
        match crate::shell_drag::drag_file(&path) {
            Ok(()) => self.say(format!("Dragged {}", file_name(&path)), Tone::Info),
            Err(e) => self.say(format!("Drag failed: {e:#}"), Tone::Error),
        }
    }

    /// Uploads the recording (always the MP4) to Imgur in a hidden browser window.
    fn start_upload(&mut self, ctx: &egui::Context) {
        if self.upload.is_some() || self.link.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        let job = crate::imgur_web::start(crate::imgur_web::Task::Upload(self.args.path.clone()), tx, move || {
            wake.request_repaint()
        });
        self.message = None;
        self.upload = Some(Upload {
            job,
            rx,
            status: "Starting…".into(),
            progress: None,
            _marker: crate::children::UploadMarker::new(),
        });
    }

    fn poll_upload(&mut self, ctx: &egui::Context) {
        use crate::imgur_web::Event;
        let Some(up) = &mut self.upload else { return };
        while let Ok(ev) = up.rx.try_recv() {
            match ev {
                Event::Status(s) => up.status = s,
                Event::Progress(p) => up.progress = Some(p),
                Event::Account(_) => {}
                Event::Done(link) => {
                    ctx.copy_text(link.clone());
                    self.copied_at = Some(Instant::now());
                    self.link = Some(link);
                    self.upload = None;
                    // The link card says it all; an older message would only compete with it.
                    self.message = None;
                    return;
                }
                Event::Failed(e) => {
                    self.upload = None;
                    if e == "cancelled" {
                        self.say("Upload cancelled.", Tone::Info);
                    } else {
                        self.say(format!("Upload failed: {e}"), Tone::Error);
                    }
                    return;
                }
            }
        }
    }

    /// Closes the window, unless an upload still needs it.
    fn close_after_save(&mut self, ctx: &egui::Context) {
        if self.upload.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        let Ok(done) = job.rx.try_recv() else { return };
        let close = job.close_when_done;
        self.job = None;
        match done {
            JobDone::Saved(p) => {
                self.saved.push(p);
                if close {
                    self.close_after_save(ctx);
                }
            }
            JobDone::Failed(m) => {
                crate::log(&m);
                self.say(m, Tone::Error);
            }
        }
    }

    // ---- Zones ----

    fn header_ui(&self, ui: &mut Ui) {
        let secs = self.args.duration_ms as f64 / 1000.0;
        let size = std::fs::metadata(&self.args.path).map(|m| m.len()).unwrap_or(0);
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.label(RichText::new("Recording finished").family(semibold()).size(18.0).color(WHITE));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Right to left: add in reverse.
                chip(ui, &format!("{:.1} MB", size as f64 / 1048576.0));
                chip(ui, &format!("{secs:.1}s"));
                chip(ui, &format!("{}×{}", self.args.width, self.args.height));
            });
        });
        ui.add_space(12.0);
    }

    fn preview_ui(&mut self, ui: &mut Ui, ctx: &egui::Context, max_h: f32) {
        let width = ui.available_width();
        // Size from the real recording; the preview frames are smaller and get scaled up.
        let (w, h) = (self.args.width.max(1) as f32, self.args.height.max(1) as f32);
        let scale = (width / w).min(max_h / h).min(1.0);
        let img = vec2(w * scale, h * scale);
        let card = vec2(width, img.y.max(MIN_PREVIEW_H.min(max_h)));
        let (card, resp) = ui.allocate_exact_size(card, Sense::click_and_drag());
        let img_rect = Rect::from_center_size(card.center(), img);
        let p = ui.painter().clone();
        p.rect_filled(card, 12, VIDEO_BG);

        let paused = self.preview.paused();
        let has_frame = match self.preview.frame(ctx).map(|t| t.id()) {
            Some(tex) => {
                let fills = img.x >= card.width() - 1.0 && img.y >= card.height() - 1.0;
                egui::Image::new((tex, img)).corner_radius(if fills { 11 } else { 6 }).paint_at(ui, img_rect);
                if paused {
                    p.circle_filled(card.center(), 28.0, Color32::from_black_alpha(160));
                    p.text(
                        card.center() + vec2(2.0, 0.0),
                        Align2::CENTER_CENTER,
                        ICON_PLAY,
                        FontId::new(22.0, icons()),
                        Color32::from_white_alpha(230),
                    );
                }
                true
            }
            None if self.preview.loading() => {
                // Painted, not added: `ui.put` would move the layout cursor into the card.
                egui::Spinner::new().size(28.0).color(WEAK).paint_at(ui, Rect::from_center_size(card.center(), vec2(28.0, 28.0)));
                false
            }
            None => {
                p.text(card.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, ICON_VIDEO, FontId::new(22.0, icons()), WEAK);
                p.text(card.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, "(no preview)", FontId::proportional(13.0), WEAK);
                false
            }
        };
        p.rect_stroke(card, 12, Stroke::new(1.0, BORDER), StrokeKind::Inside);

        let cursor = if resp.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab };
        let resp = resp.on_hover_cursor(cursor);
        if resp.drag_started() {
            self.start_drag(ctx);
        } else if resp.clicked() && has_frame {
            self.preview.toggle_pause();
            ctx.request_repaint();
        }

        // Drag format pill, on top of the preview so clicks on it don't pause playback.
        let label = match self.drag_format {
            DragFormat::Mp4 => "Drag: MP4",
            DragFormat::Gif => "Drag: GIF",
        };
        let text = p.layout_no_wrap(label.to_owned(), FontId::proportional(12.5), Color32::PLACEHOLDER);
        let chevron = p.layout_no_wrap(ICON_CHEVRON.to_owned(), FontId::new(9.0, icons()), Color32::PLACEHOLDER);
        let pill_size = vec2(text.size().x + chevron.size().x + 28.0, 26.0);
        let pill = Rect::from_min_size(Pos2::new(card.right() - 10.0 - pill_size.x, card.top() + 10.0), pill_size);
        let pill_resp = ui
            .interact(pill, ui.id().with("drag-format"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Which file dragging the preview hands out");
        let hover = ui.ctx().animate_bool_with_time(pill_resp.id, pill_resp.hovered(), 0.12);
        p.rect(
            pill,
            13,
            Color32::from_black_alpha((170.0 + 50.0 * hover) as u8),
            Stroke::new(1.0, Color32::from_white_alpha((28.0 + 30.0 * hover) as u8)),
            StrokeKind::Inside,
        );
        let tx = pill.left() + 12.0 + text.size().x / 2.0;
        let cx = pill.right() - 12.0 - chevron.size().x / 2.0;
        p.galley(centered_galley_pos(&text, Pos2::new(tx, pill.center().y)), text, WHITE);
        p.galley(centered_galley_pos(&chevron, Pos2::new(cx, pill.center().y)), chevron, Color32::from_white_alpha(200));
        egui::Popup::menu(&pill_resp).align(egui::RectAlign::BOTTOM_END).gap(4.0).show(|ui| {
            ui.set_min_width(110.0);
            ui.selectable_value(&mut self.drag_format, DragFormat::Mp4, "Drag MP4");
            ui.selectable_value(&mut self.drag_format, DragFormat::Gif, "Drag GIF");
        });

        // Hint along the bottom edge. Painted only, so clicks and drags reach the preview.
        if has_frame {
            let hint = p.layout_no_wrap(
                "Drag preview to drop file anywhere  •  Click to pause".to_owned(),
                FontId::proportional(14.0),
                Color32::PLACEHOLDER,
            );
            let size = vec2(hint.size().x + 32.0, 32.0);
            if size.x + 20.0 <= card.width() {
                let rect = Rect::from_center_size(Pos2::new(card.center().x, card.bottom() - 12.0 - size.y / 2.0), size);
                p.rect(rect, 16, Color32::from_black_alpha(185), Stroke::new(1.0, Color32::from_white_alpha(28)), StrokeKind::Inside);
                // Up a little so the descenders (g, p, y) don't make the line look low.
                p.galley(centered_galley_pos(&hint, rect.center() - vec2(0.0, 2.0)), hint, Color32::from_white_alpha(240));
            }
        }
    }

    fn actions_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let busy = self.job.is_some();
        let (up_icon, up_text) = if self.link.is_some() {
            (ICON_CHECK, "Uploaded")
        } else if self.upload.is_some() {
            (ICON_UPLOAD, "Uploading…")
        } else {
            (ICON_UPLOAD, "Upload to Imgur")
        };
        let mp4_icon = if self.saved_with_ext("mp4").is_some() { ICON_CHECK } else { ICON_SAVE };
        let gif_icon = if self.saved_with_ext("gif").is_some() { ICON_CHECK } else { ICON_PICTURE };

        // Upload takes half the row, each save a quarter.
        let gap = ui.spacing().item_spacing.x;
        let quarter = ((ui.available_width() - gap * 2.0) / 4.0).floor();
        let half = ui.available_width() - gap * 2.0 - quarter * 2.0;
        let fits = button_width(ui, mp4_icon, "Save MP4", Kind::Secondary, 34.0) <= quarter;
        let (mp4, gif) = if fits { ("Save MP4", "Save GIF") } else { ("MP4", "GIF") };

        ui.horizontal(|ui| {
            let can_upload = self.upload.is_none() && self.link.is_none();
            if button_with(ui, up_icon, up_text, Kind::Primary, can_upload, 34.0, Some(half)).clicked() {
                self.start_upload(ctx);
            }
            if button_with(ui, mp4_icon, mp4, Kind::Secondary, !busy, 34.0, Some(quarter)).clicked() {
                self.save_mp4(ctx, true);
            }
            if button_with(ui, gif_icon, gif, Kind::Secondary, !busy, 34.0, Some(quarter)).clicked() {
                self.save_gif(ctx, true);
            }
        });
    }

    fn has_activity(&self) -> bool {
        self.job.is_some() || self.upload.is_some() || self.link.is_some()
    }

    fn activity_ui(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        if let Some(up) = &self.upload {
            upload_ui(ui, up);
        }
        if let Some(job) = &self.job {
            self.job_ui(ui, job);
        }
        if let Some(link) = self.link.clone() {
            self.link_ui(ui, &link);
        }
    }

    fn job_ui(&self, ui: &mut Ui, job: &Job) {
        let g = &job.progress.gif;
        let expected = (self.args.duration_ms * self.cfg.gif_fps as u64 / 1000).max(1);
        let out = g.frames_out.load(Ordering::Relaxed) as u64;
        let started = out > 0 || g.frames_in.load(Ordering::Relaxed) > 0;
        let frac = (job.gif && started).then(|| (out as f32 / expected as f32).min(1.0));
        panel(ui, CARD, BORDER, |ui| {
            ui.horizontal(|ui| {
                glyph(ui, if job.gif { ICON_PICTURE } else { ICON_SAVE }, 14.0, ACCENT);
                ui.label(RichText::new(if job.gif { "Making GIF…" } else { "Saving MP4…" }).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if job.gif && icon_button(ui, ICON_CLOSE, "Cancel").clicked() {
                        g.cancel.store(true, Ordering::Relaxed);
                    }
                    if frac.is_some() {
                        ui.label(RichText::new(format!("frame {out} / ~{expected}")).size(12.0).color(WEAK));
                    }
                });
            });
            ui.add_space(2.0);
            progress_bar(ui, frac);
        });
    }

    fn link_ui(&mut self, ui: &mut Ui, link: &str) {
        let copied = self.copied_at.is_some_and(|t| t.elapsed() < COPIED_TIME);
        if let Some(t) = self.copied_at.filter(|_| copied) {
            ui.ctx().request_repaint_after(COPIED_TIME.saturating_sub(t.elapsed()));
        }
        panel(ui, SUCCESS_FILL, SUCCESS.gamma_multiply(0.45), |ui| {
            ui.horizontal(|ui| {
                glyph(ui, ICON_CHECK, 14.0, SUCCESS);
                ui.label(RichText::new("Uploaded to Imgur").family(semibold()).color(WHITE));
                ui.label(RichText::new("•  Link copied to the clipboard").size(13.0).color(WEAK));
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Same width either way, so the link beside it doesn't shift.
                    let width = button_width(ui, ICON_COPY, "Copy link", Kind::Secondary, 28.0);
                    let (icon, text) = if copied { (ICON_CHECK, "Copied") } else { (ICON_COPY, "Copy link") };
                    if button_with(ui, icon, text, Kind::Secondary, true, 28.0, Some(width)).clicked() {
                        ui.ctx().copy_text(link.to_owned());
                        self.copied_at = Some(Instant::now());
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        egui::Frame::new()
                            .fill(FIELD)
                            .stroke(Stroke::new(1.0, BORDER))
                            .corner_radius(6)
                            .inner_margin(Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let r = ui
                                    .add(egui::Label::new(RichText::new(link).color(INFO)).truncate().sense(Sense::click()))
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text("Open in your browser");
                                if r.clicked() {
                                    let _ = open::that(link);
                                }
                            });
                    });
                });
            });
        });
    }

    fn message_ui(&mut self, ui: &mut Ui) {
        let Some(m) = &self.message else { return };
        let (fill, stroke, fg, icon, icon_color) = match m.tone {
            Tone::Info => (INFO_FILL, INFO_BORDER, TEXT, ICON_INFO, INFO),
            Tone::Error => (ERR_FILL, ERR_BORDER, ERR_TEXT, ICON_ERROR, ERR_TEXT),
        };
        let text = m.text.clone();
        let mut dismiss = false;
        ui.add_space(10.0);
        panel(ui, fill, stroke, |ui| {
            ui.horizontal(|ui| {
                glyph(ui, icon, 14.0, icon_color);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    dismiss = icon_button(ui, ICON_CLOSE, "Dismiss").clicked();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).size(13.0).color(fg)).wrap());
                    });
                });
            });
        });
        if dismiss {
            self.message = None;
        }
    }

    /// Modal asking before throwing away the recording or a running job.
    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let (title, body, confirm) = if self.upload.is_some() {
            ("Upload in progress", "The Imgur upload is still running. Close anyway?", "Close anyway")
        } else if self.job.as_ref().is_some_and(|j| j.gif) {
            ("Export in progress", "A GIF export is still running. Close anyway?", "Close anyway")
        } else if self.job.is_some() {
            ("Save in progress", "The MP4 is still being saved. Close anyway?", "Close anyway")
        } else {
            ("Unsaved recording", "This recording hasn't been saved. Discard it?", "Discard")
        };
        let frame = egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(14)
            .inner_margin(Margin::same(20))
            .shadow(Shadow { offset: [0, 10], blur: 32, spread: 0, color: Color32::from_black_alpha(140) });
        let mut keep = false;
        let mut discard = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-discard"))
            .backdrop_color(Color32::from_black_alpha(178))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width((ctx.screen_rect().width() - 80.0).clamp(240.0, 340.0));
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 16.0, AMBER.gamma_multiply(0.14));
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, ICON_WARNING, FontId::new(15.0, icons()), AMBER);
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).family(semibold()).size(16.0).color(WHITE));
                });
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(body).color(WEAK)).wrap());
                ui.add_space(18.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    discard = button(ui, "", confirm, Kind::Danger, true).clicked();
                    keep = button(ui, "", "Keep open", Kind::Secondary, true).clicked();
                });
            });
        if discard {
            if let Some(j) = &self.job {
                j.progress.gif.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(up) = &self.upload {
                up.job.cancel();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if keep || modal.should_close() {
            self.confirm_discard = false;
        }
    }
}

impl eframe::App for Popup {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.poll_upload(ctx);
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if !self.ran_default {
            self.ran_default = true;
            match self.cfg.default_action {
                DefaultAction::Ask => {}
                DefaultAction::SaveMp4 => self.save_mp4(ctx, false),
                DefaultAction::SaveGif => self.save_gif(ctx, false),
                DefaultAction::Upload => self.start_upload(ctx),
            }
        }

        // Ask before throwing away a recording that was never saved or uploaded.
        if ctx.input(|i| i.viewport().close_requested()) {
            let nothing_done = self.saved.is_empty() && self.link.is_none();
            let busy = self.job.is_some() || self.upload.is_some();
            if (nothing_done || busy) && !self.confirm_discard {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_discard = true;
            }
        }

        // The window's height follows its content, up to nearly the screen's height; past that
        // the preview shrinks instead.
        let (inner, maximized, monitor_h) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.maximized.unwrap_or(false), v.monitor_size.map_or(1080.0, |s| s.y))
        });
        let max_h = monitor_h - 120.0;
        let margin_y = 2.0 * MARGIN.1 as f32;

        let panel_frame = egui::Frame::new().fill(CANVAS).inner_margin(Margin::symmetric(MARGIN.0, MARGIN.1));
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            let start = ui.cursor().top();
            self.header_ui(ui);
            let header_h = ui.cursor().top() - start;
            let preview_max = if maximized { ui.available_height() } else { max_h - margin_y - header_h };
            self.preview_ui(ui, ctx, (preview_max - self.below_h).max(MIN_PREVIEW_H));

            let top = ui.cursor().top();
            ui.add_space(10.0);
            self.actions_ui(ui, ctx);
            if self.has_activity() {
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .max_height((max_h * 0.4).max(90.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| self.activity_ui(ui));
            }
            self.message_ui(ui);
            // Everything under the preview, for next frame's preview size.
            let below = ui.cursor().top() - top;
            if (below - self.below_h).abs() > 0.5 {
                self.below_h = below;
                ctx.request_discard("popup layout");
            }

            let want = (ui.cursor().top() - start + margin_y).ceil();
            let off = inner.is_some_and(|r| (r.height() - want).abs() > 1.0);
            if !self.placed {
                // While the window opens: fit it and center it in one go. The first frames can
                // measure a different height, so center again until the size holds.
                if let Some(hwnd) = self.hwnd.filter(|_| self.placed_for != Some(want)) {
                    place_window(hwnd, (want * ctx.pixels_per_point()).round() as i32);
                    self.placed_for = Some(want);
                }
                self.placed = self.hwnd.is_none() || (!off && self.placed_for == Some(want));
            } else if let Some(inner) = inner.filter(|_| !maximized && off) {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(inner.width(), want)));
            }
        });

        if self.confirm_discard {
            self.confirm_ui(ctx);
        }
    }
}

fn window_handle(cc: &eframe::CreationContext) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Gives the window `client_h` pixels of content and centers it on the monitor under the
/// cursor, which is where the recording was just made.
fn place_window(hwnd: isize, client_h: i32) {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let (mut outer, mut client) = (RECT::default(), RECT::default());
        if GetWindowRect(hwnd, &mut outer).is_err() || GetClientRect(hwnd, &mut client).is_err() {
            return;
        }
        // The visible frame, without Windows 10/11's invisible resize borders.
        let mut frame = outer;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        let w = outer.right - outer.left;
        let h = outer.bottom - outer.top - (client.bottom - client.top) + client_h;
        let (vis_w, vis_h) = (frame.right - frame.left, h - (frame.top - outer.top) - (outer.bottom - frame.bottom));

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return;
        }
        let a = mi.rcWork;
        let x = a.left + ((a.right - a.left) - vis_w) / 2 - (frame.left - outer.left);
        let y = a.top + ((a.bottom - a.top) - vis_h).max(0) / 2 - (frame.top - outer.top);
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The upload's progress card, or an amber banner while Imgur waits on the user.
fn upload_ui(ui: &mut Ui, up: &Upload) {
    let needs_user = up.status.starts_with("Sign in to Imgur") || up.status.starts_with("Imgur needs something");
    if needs_user {
        panel(ui, AMBER_FILL, AMBER_BORDER, |ui| {
            ui.horizontal(|ui| {
                glyph(ui, ICON_WARNING, 14.0, AMBER);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, ICON_CLOSE, "Cancel upload").clicked() {
                        up.job.cancel();
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(&up.status).color(AMBER)).wrap());
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                if small_button(ui, ICON_OPEN, "Bring browser to front", Kind::Secondary, true).clicked() {
                    up.job.bring_to_front();
                }
            });
        });
        return;
    }
    let pct = up.progress.filter(|p| *p < 1.0);
    panel(ui, CARD, BORDER, |ui| {
        ui.horizontal(|ui| {
            glyph(ui, ICON_UPLOAD, 14.0, ACCENT);
            let label = match pct {
                Some(p) => format!("Uploading… {:.0}%", p * 100.0),
                None => up.status.clone(),
            };
            ui.label(RichText::new(label).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, ICON_CLOSE, "Cancel upload").clicked() {
                    up.job.cancel();
                }
            });
        });
        ui.add_space(2.0);
        progress_bar(ui, pct);
        if pct.is_some() && up.status != "Uploading…" {
            ui.label(RichText::new(&up.status).size(12.0).color(WEAK));
        }
    });
}

// ---- Look ----

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = CANVAS;
        v.window_fill = CARD;
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = CARD;
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        let shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, WHITE);
        v.hyperlink_color = INFO;

        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_stroke = Stroke::NONE;
        w.hovered.fg_stroke = Stroke::new(1.0, WHITE);
        w.active.weak_bg_fill = HOVER;
        w.active.fg_stroke = Stroke::new(1.0, WHITE);
        for w in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            w.corner_radius = CornerRadius::same(6);
            w.expansion = 0.0;
        }

        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 6.0);
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.animation_time = 0.15;
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(14.0));
        s.text_styles.insert(Button, FontId::proportional(14.0));
        s.text_styles.insert(Small, FontId::proportional(12.0));
        s.text_styles.insert(Heading, FontId::new(18.0, semibold()));
    });
}

fn panel<R>(ui: &mut Ui, fill: Color32, stroke: Color32, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

fn glyph(ui: &mut Ui, icon: &str, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::new(size, icons()), color);
}

/// Header detail, e.g. the resolution.
fn chip(ui: &mut Ui, text: &str) {
    let g = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.0), Color32::PLACEHOLDER);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 16.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, 11, HOVER);
    ui.painter().galley(centered_galley_pos(&g, rect.center()), g, TEXT);
}

/// Thin bar; `None` shows a segment sweeping across while the end is unknown.
fn progress_bar(ui: &mut Ui, frac: Option<f32>) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3, BORDER);
    match frac {
        Some(f) => {
            let f = ui.ctx().animate_value_with_time(resp.id, f, 0.25);
            if f > 0.0 {
                p.rect_filled(Rect::from_min_size(rect.min, vec2((rect.width() * f).max(6.0), rect.height())), 3, ACCENT);
            }
        }
        None => {
            let t = (ui.input(|i| i.time) % 1.6 / 1.6) as f32;
            let t = t * t * (3.0 - 2.0 * t);
            let seg = rect.width() * 0.3;
            let x = rect.left() - seg + (rect.width() + seg) * t;
            let bar = Rect::from_x_y_ranges(x.max(rect.left())..=(x + seg).min(rect.right()), rect.y_range());
            if bar.width() > 0.0 {
                p.rect_filled(bar, 3, ACCENT);
            }
            ui.ctx().request_repaint();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

fn button(ui: &mut Ui, icon: &str, text: &str, kind: Kind, enabled: bool) -> egui::Response {
    button_with(ui, icon, text, kind, enabled, 34.0, None)
}

fn small_button(ui: &mut Ui, icon: &str, text: &str, kind: Kind, enabled: bool) -> egui::Response {
    button_with(ui, icon, text, kind, enabled, 28.0, None)
}

fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> egui::Response {
    button_with(ui, icon, "", Kind::Ghost, true, 24.0, None).on_hover_text(tooltip)
}

fn button_galleys(ui: &Ui, icon: &str, text: &str, h: f32) -> (Option<Arc<egui::Galley>>, Option<Arc<egui::Galley>>, f32) {
    let size = if h < 30.0 { 13.0 } else { 14.0 };
    let layout = |s: &str, font: FontId| {
        (!s.is_empty()).then(|| ui.painter().layout_no_wrap(s.to_owned(), font, Color32::PLACEHOLDER))
    };
    let icon_g = layout(icon, FontId::new(size, icons()));
    let text_g = layout(text, FontId::proportional(size));
    let gap = if icon_g.is_some() && text_g.is_some() { 8.0 } else { 0.0 };
    (icon_g, text_g, gap)
}

fn button_width(ui: &Ui, icon: &str, text: &str, kind: Kind, h: f32) -> f32 {
    let (icon_g, text_g, gap) = button_galleys(ui, icon, text, h);
    let pad = if h < 30.0 || kind == Kind::Ghost { 20.0 } else { 28.0 };
    match text_g {
        None => h,
        Some(t) => icon_g.map_or(0.0, |g| g.size().x) + gap + t.size().x + pad,
    }
}

/// Icon and/or text button; icon-only ones are square. `width` overrides the natural width.
fn button_with(ui: &mut Ui, icon: &str, text: &str, kind: Kind, enabled: bool, h: f32, width: Option<f32>) -> egui::Response {
    let width = width.unwrap_or_else(|| button_width(ui, icon, text, kind, h));
    let (icon_g, text_g, gap) = button_galleys(ui, icon, text, h);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.12);
    let rect = if enabled && resp.is_pointer_button_down_on() { rect.shrink(0.5) } else { rect };
    let (fill, stroke, fg) = match kind {
        Kind::Primary => (mix(ACCENT, ACCENT_HOVER, hover), Color32::TRANSPARENT, WHITE),
        Kind::Secondary => (mix(SURFACE, HOVER, hover), mix(SURFACE_BORDER, BORDER, hover), TEXT),
        Kind::Ghost => (mix(Color32::TRANSPARENT, HOVER, hover), Color32::TRANSPARENT, mix(WEAK, WHITE, hover)),
        Kind::Danger => (mix(DANGER, DANGER_HOVER, hover), Color32::TRANSPARENT, WHITE),
    };
    let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
    let p = ui.painter();
    p.rect(rect, 8, dim(fill), Stroke::new(1.0, dim(stroke)), StrokeKind::Inside);
    let content = icon_g.as_ref().map_or(0.0, |g| g.size().x) + gap + text_g.as_ref().map_or(0.0, |g| g.size().x);
    let mut x = rect.center().x - content / 2.0;
    for g in [icon_g, text_g].into_iter().flatten() {
        let w = g.size().x;
        p.galley(centered_galley_pos(&g, Pos2::new(x + w / 2.0, rect.center().y)), g, dim(fg));
        x += w + gap;
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}
