//! Post-recording popup (runs as `screen2imgur.exe popup <file> <w> <h> <ms>`).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::config::{Config, DefaultAction, DragFormat};
use crate::gif::{GifOptions, Progress};

pub struct Args {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

const IMGUR_UPLOAD_URL: &str = "https://imgur.com/upload";

pub fn run(args: Args) -> Result<()> {
    if !args.path.exists() {
        return Err(anyhow!("{} does not exist", args.path.display()));
    }
    let temp_mp4 = args.path.clone();
    let temp_gif = temp_mp4.with_extension("gif");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screen2imgur — recording")
            .with_inner_size([560.0, 560.0])
            .with_min_inner_size([420.0, 380.0])
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
}

struct Popup {
    args: Args,
    cfg: Config,
    base_name: String,
    preview: crate::preview::Preview,
    drag_format: DragFormat,
    job: Option<Job>,
    saved: Vec<PathBuf>,
    message: Option<(String, bool)>,
    confirm_discard: bool,
    ran_default: bool,
}

impl Popup {
    fn new(cc: &eframe::CreationContext, args: Args) -> Self {
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
            message: None,
            confirm_discard: false,
            ran_default: false,
        }
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

    fn spawn(&mut self, ctx: &egui::Context, work: impl FnOnce(&JobProgress) -> JobDone + Send + 'static) {
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
        self.job = Some(Job { progress, rx });
    }

    fn saved_with_ext(&self, ext: &str) -> Option<PathBuf> {
        self.saved.iter().find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))).cloned()
    }

    fn save_mp4(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.saved_with_ext("mp4") {
            self.message = Some((format!("Already saved as {}", p.display()), false));
            return;
        }
        let src = self.args.path.clone();
        let dst = self.output_path("mp4");
        self.spawn(ctx, move |_| {
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

    fn save_gif(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.saved_with_ext("gif") {
            self.message = Some((format!("Already saved as {}", p.display()), false));
            return;
        }
        let src = self.args.path.clone();
        let dst = self.output_path("gif");
        let crop = (self.args.width, self.args.height);
        let opts = self.gif_options();
        self.spawn(ctx, move |p| {
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
                        self.message = Some((format!("Saving MP4 failed: {e}"), true));
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
                        self.save_gif(ctx);
                    }
                    self.message = Some(("Making the GIF first. Drag again once it's saved.".into(), false));
                    return;
                }
            },
        };
        match crate::shell_drag::drag_file(&path) {
            Ok(()) => self.message = Some((format!("Dragged {}", path.display()), false)),
            Err(e) => self.message = Some((format!("Drag failed: {e:#}"), true)),
        }
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(done) = job.rx.try_recv() else { return };
        self.job = None;
        match done {
            JobDone::Saved(p) => {
                self.message = Some((format!("Saved {}", p.display()), false));
                self.saved.push(p);
            }
            JobDone::Failed(m) => {
                crate::log(&m);
                self.message = Some((m, true));
            }
        }
    }

    fn progress_ui(&self, ui: &mut egui::Ui, job: &Job) {
        let p = &job.progress;
        let expected = (self.args.duration_ms * self.cfg.gif_fps as u64 / 1000).max(1);
        let out = p.gif.frames_out.load(Ordering::Relaxed) as u64;
        let (frac, text) = if out == 0 && p.gif.frames_in.load(Ordering::Relaxed) == 0 {
            (0.0, "Working…".to_string())
        } else {
            ((out as f32 / expected as f32).min(1.0), format!("Making GIF… frame {out} / ~{expected}"))
        };
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).text(text).desired_width(ui.available_width() - 80.0));
            if ui.button("Cancel").clicked() {
                p.gif.cancel.store(true, Ordering::Relaxed);
            }
        });
    }
}

impl eframe::App for Popup {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if !self.ran_default {
            self.ran_default = true;
            match self.cfg.default_action {
                DefaultAction::Ask => {}
                DefaultAction::SaveMp4 => self.save_mp4(ctx),
                DefaultAction::SaveGif => self.save_gif(ctx),
            }
        }

        // Ask before throwing away a recording that was never saved.
        if ctx.input(|i| i.viewport().close_requested()) {
            let nothing_done = self.saved.is_empty();
            if (nothing_done || self.job.is_some()) && !self.confirm_discard {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_discard = true;
            }
        }

        let secs = self.args.duration_ms as f64 / 1000.0;
        let size = std::fs::metadata(&self.args.path).map(|m| m.len()).unwrap_or(0);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Recording finished");
                ui.weak(format!(
                    "{}×{} · {:.1} s · {:.1} MB",
                    self.args.width, self.args.height, secs, size as f64 / 1048576.0
                ));
            });
            ui.add_space(6.0);

            let avail = ui.available_width();
            let max_h = (ui.available_height() - 190.0).max(120.0);
            // Size from the real recording; the preview frames are smaller and get scaled up.
            let (w, h) = (self.args.width.max(1) as f32, self.args.height.max(1) as f32);
            let scale = (avail / w).min(max_h / h).min(1.0);
            let size = egui::vec2(w * scale, h * scale);
            let paused = self.preview.paused();
            match self.preview.frame(ctx).map(|t| t.id()) {
                Some(tex) => {
                    let resp = ui
                        .vertical_centered(|ui| {
                            ui.add(egui::Image::new((tex, size)).sense(egui::Sense::click_and_drag()))
                        })
                        .inner;
                    if paused {
                        ui.painter().text(
                            resp.rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "⏸",
                            egui::FontId::proportional(48.0),
                            egui::Color32::from_white_alpha(200),
                        );
                    }
                    let resp = resp.on_hover_cursor(egui::CursorIcon::Grab).on_hover_text(
                        "Drag onto the Imgur upload page, Discord, a folder… (saves the file first). Click to pause.",
                    );
                    if resp.drag_started() {
                        self.start_drag(ctx);
                    } else if resp.clicked() {
                        self.preview.toggle_pause();
                        ctx.request_repaint();
                    }
                    ui.vertical_centered(|ui| {
                        ui.weak("Drag the preview to drop the file anywhere · click to pause");
                    });
                }
                None if self.preview.loading() => {
                    ui.vertical_centered(|ui| ui.spinner());
                }
                None => {
                    ui.weak("(no preview)");
                }
            }
            ui.add_space(8.0);

            let busy = self.job.is_some();
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("💾 Save MP4")).clicked() {
                    self.save_mp4(ctx);
                }
                if ui.add_enabled(!busy, egui::Button::new("🎞 Save GIF")).clicked() {
                    self.save_gif(ctx);
                }
                ui.separator();
                if ui.button("🌐 Open Imgur upload").clicked() {
                    let _ = open::that(IMGUR_UPLOAD_URL);
                    self.message = Some(("Drag the preview onto the Imgur page to upload it.".into(), false));
                }
                egui::ComboBox::from_id_salt("fmt")
                    .width(90.0)
                    .selected_text(match self.drag_format {
                        DragFormat::Mp4 => "drag MP4",
                        DragFormat::Gif => "drag GIF",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.drag_format, DragFormat::Mp4, "drag MP4");
                        ui.selectable_value(&mut self.drag_format, DragFormat::Gif, "drag GIF");
                    });
            });

            if let Some(job) = &self.job {
                ui.add_space(4.0);
                self.progress_ui(ui, job);
            }

            for p in self.saved.clone() {
                ui.horizontal(|ui| {
                    ui.weak(p.display().to_string());
                    if ui.small_button("Open").clicked() {
                        let _ = open::that(&p);
                    }
                    if ui.small_button("Show in folder").clicked() {
                        let _ = std::process::Command::new("explorer").arg("/select,").arg(&p).spawn();
                    }
                });
            }

            if let Some((m, err)) = &self.message {
                ui.add_space(4.0);
                let c = if *err { egui::Color32::from_rgb(230, 80, 70) } else { ui.visuals().weak_text_color() };
                ui.colored_label(c, m);
            }

            if self.confirm_discard {
                ui.add_space(8.0);
                ui.group(|ui| {
                    ui.label(if self.job.is_some() {
                        "A GIF export is still running. Close anyway?"
                    } else {
                        "This recording hasn't been saved. Discard it?"
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Discard").clicked() {
                            if let Some(j) = &self.job {
                                j.progress.gif.cancel.store(true, Ordering::Relaxed);
                            }
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if ui.button("Keep open").clicked() {
                            self.confirm_discard = false;
                        }
                    });
                });
            }

            ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}
