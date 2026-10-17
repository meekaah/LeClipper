//! Post-recording popup (runs as `screen2imgur.exe popup <file> <w> <h> <ms>`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::config::{Config, DefaultAction, UploadFormat};
use crate::gif::{GifOptions, Progress};
use crate::imgur::Uploaded;

pub struct Args {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

const IMGUR_MAX_SECS: u64 = 60;
const IMGUR_MAX_BYTES: u64 = 200 * 1024 * 1024;

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
    sent: Arc<AtomicU64>,
    total: AtomicU64,
    /// 0 = making GIF, 1 = uploading.
    phase: AtomicU8,
}

enum JobDone {
    Saved(PathBuf),
    Uploaded(Uploaded),
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
    thumb: Option<egui::TextureHandle>,
    thumb_rx: Option<Receiver<Option<egui::ColorImage>>>,
    upload_format: UploadFormat,
    job: Option<Job>,
    saved: Vec<PathBuf>,
    uploaded: Option<Uploaded>,
    message: Option<(String, bool)>,
    confirm_discard: bool,
    ran_default: bool,
}

impl Popup {
    fn new(cc: &eframe::CreationContext, args: Args) -> Self {
        let cfg = Config::load();
        let (tx, rx) = mpsc::channel();
        let path = args.path.clone();
        let crop = (args.width, args.height);
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let mut img = None;
            let _ = crate::decode::for_each_frame(&path, Some(crop), |f| {
                img = Some(egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba));
                Ok(false)
            });
            let _ = tx.send(img);
            ctx.request_repaint();
        });
        Self {
            base_name: format!("screen2imgur_{}", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")),
            upload_format: cfg.upload_format,
            cfg,
            args,
            thumb: None,
            thumb_rx: Some(rx),
            job: None,
            saved: Vec::new(),
            uploaded: None,
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

    fn save_mp4(&mut self, ctx: &egui::Context) {
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
        let src = self.args.path.clone();
        let temp_gif = src.with_extension("gif");
        let dst = self.output_path("gif");
        let crop = (self.args.width, self.args.height);
        let opts = self.gif_options();
        self.spawn(ctx, move |p| {
            // Reuse a GIF already made for an upload.
            let r = if temp_gif.exists() {
                std::fs::create_dir_all(dst.parent().unwrap_or(Path::new(".")))
                    .and_then(|_| std::fs::copy(&temp_gif, &dst))
                    .map(|_| ())
                    .map_err(Into::into)
            } else {
                crate::gif::mp4_to_gif(&src, &dst, crop, &opts, p.gif.clone())
            };
            match r {
                Ok(()) => JobDone::Saved(dst),
                Err(e) => JobDone::Failed(format!("GIF export failed: {e:#}")),
            }
        });
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let src = self.args.path.clone();
        let temp_gif = src.with_extension("gif");
        let crop = (self.args.width, self.args.height);
        let opts = self.gif_options();
        let format = self.upload_format;
        let album = self.cfg.imgur_album_id.clone();
        let title = self.base_name.clone();
        self.spawn(ctx, move |p| {
            let r = (|| -> Result<Uploaded> {
                let file = match format {
                    UploadFormat::Mp4 => src.clone(),
                    UploadFormat::Gif => {
                        if !temp_gif.exists() {
                            crate::gif::mp4_to_gif(&src, &temp_gif, crop, &opts, p.gif.clone())?;
                        }
                        temp_gif.clone()
                    }
                };
                let len = std::fs::metadata(&file)?.len();
                if len > IMGUR_MAX_BYTES {
                    return Err(anyhow!("file is {:.0} MB; Imgur's limit is 200 MB", len as f64 / 1048576.0));
                }
                p.total.store(len, Ordering::Relaxed);
                p.phase.store(1, Ordering::Relaxed);
                crate::imgur::upload(&file, &title, album.as_deref(), p.sent.clone())
            })();
            match r {
                Ok(u) => JobDone::Uploaded(u),
                Err(e) => JobDone::Failed(format!("Upload failed: {e:#}")),
            }
        });
    }

    fn link(&self, u: &Uploaded) -> String {
        if self.cfg.copy_direct_link { u.direct_link.clone() } else { u.page_link.clone() }
    }

    fn copy_link(&mut self, link: &str) {
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(link.to_string())) {
            Ok(()) => self.message = Some(("Link copied to the clipboard.".into(), false)),
            Err(e) => self.message = Some((format!("Couldn't copy: {e}"), true)),
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
            JobDone::Uploaded(u) => {
                crate::log(&format!("uploaded {} as {} (deletehash {})", u.id, u.page_link, u.deletehash));
                let link = self.link(&u);
                self.uploaded = Some(u);
                if self.cfg.auto_copy_link {
                    self.copy_link(&link);
                } else {
                    self.message = Some(("Uploaded.".into(), false));
                }
            }
            JobDone::Failed(m) => {
                crate::log(&m);
                self.message = Some((m, true));
            }
        }
    }

    fn progress_ui(&self, ui: &mut egui::Ui, job: &Job) {
        let p = &job.progress;
        let (frac, text) = if p.phase.load(Ordering::Relaxed) == 1 {
            let total = p.total.load(Ordering::Relaxed).max(1);
            let sent = p.sent.load(Ordering::Relaxed);
            (sent as f32 / total as f32, format!("Uploading… {:.1} / {:.1} MB", sent as f64 / 1048576.0, total as f64 / 1048576.0))
        } else {
            let expected = (self.args.duration_ms * self.cfg.gif_fps as u64 / 1000).max(1);
            let out = p.gif.frames_out.load(Ordering::Relaxed) as u64;
            if out == 0 && p.gif.frames_in.load(Ordering::Relaxed) == 0 {
                (0.0, "Working…".to_string())
            } else {
                ((out as f32 / expected as f32).min(1.0), format!("Making GIF… frame {out} / ~{expected}"))
            }
        };
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).text(text).desired_width(ui.available_width() - 80.0));
            if p.phase.load(Ordering::Relaxed) == 0 && ui.button("Cancel").clicked() {
                p.gif.cancel.store(true, Ordering::Relaxed);
            }
        });
    }
}

impl eframe::App for Popup {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.thumb_rx {
            if let Ok(img) = rx.try_recv() {
                self.thumb_rx = None;
                self.thumb = img.map(|i| ctx.load_texture("thumb", i, egui::TextureOptions::LINEAR));
            }
        }
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
                DefaultAction::Upload => self.upload(ctx),
            }
        }

        // Ask before throwing away a recording that was never saved or uploaded.
        if ctx.input(|i| i.viewport().close_requested()) {
            let nothing_done = self.saved.is_empty() && self.uploaded.is_none();
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
            match &self.thumb {
                Some(t) => {
                    let [w, h] = t.size_vec2().into();
                    let scale = (avail / w).min(max_h / h).min(1.0);
                    ui.vertical_centered(|ui| {
                        ui.image((t.id(), egui::vec2(w * scale, h * scale)));
                    });
                }
                None if self.thumb_rx.is_some() => {
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
                if ui.add_enabled(!busy, egui::Button::new("☁ Upload to Imgur")).clicked() {
                    self.upload(ctx);
                }
                egui::ComboBox::from_id_salt("fmt")
                    .width(70.0)
                    .selected_text(match self.upload_format {
                        UploadFormat::Mp4 => "as MP4",
                        UploadFormat::Gif => "as GIF",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.upload_format, UploadFormat::Mp4, "as MP4");
                        ui.selectable_value(&mut self.upload_format, UploadFormat::Gif, "as GIF");
                    });
            });
            if secs > IMGUR_MAX_SECS as f64 && self.upload_format == UploadFormat::Mp4 {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 160, 40),
                    format!("Imgur may reject videos longer than {IMGUR_MAX_SECS} s."),
                );
            }

            if let Some(job) = &self.job {
                ui.add_space(4.0);
                self.progress_ui(ui, job);
            }

            if let Some(u) = self.uploaded.clone() {
                ui.add_space(6.0);
                let link = self.link(&u);
                ui.horizontal(|ui| {
                    ui.hyperlink_to(&link, &link);
                    if ui.button("📋 Copy link").clicked() {
                        self.copy_link(&link);
                    }
                });
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
                        "A save/upload is still running. Close anyway?"
                    } else {
                        "This recording hasn't been saved or uploaded. Discard it?"
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
