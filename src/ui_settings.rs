//! Settings window (runs as `screen2imgur.exe settings`).

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::config::{Config, DefaultAction, UploadFormat};
use crate::imgur::{self, Album};
use crate::secrets::{self, Tokens};

pub fn app_icon() -> egui::IconData {
    egui::IconData { rgba: crate::icon::rgba(false), width: crate::icon::SIZE, height: crate::icon::SIZE }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screen2imgur settings")
            .with_inner_size([540.0, 700.0])
            .with_min_inner_size([420.0, 400.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native("screen2imgur settings", options, Box::new(|_cc| Ok(Box::new(SettingsApp::new()))))
        .map_err(|e| anyhow!("{e}"))
}

struct SettingsApp {
    cfg: Config,
    client_id: String,
    client_secret: String,
    account: Option<String>,
    login: Option<(Receiver<Result<Tokens>>, Arc<AtomicBool>)>,
    albums: Vec<Album>,
    albums_job: Option<Receiver<Result<Vec<Album>>>>,
    status: Option<(String, bool)>,
}

impl SettingsApp {
    fn new() -> Self {
        Self {
            cfg: Config::load(),
            client_id: secrets::client_id().unwrap_or_default(),
            client_secret: secrets::client_secret().unwrap_or_default(),
            account: secrets::tokens().map(|t| t.account_username),
            login: None,
            albums: Vec::new(),
            albums_job: None,
            status: None,
        }
    }

    fn hotkey_error(&self) -> Option<String> {
        global_hotkey::hotkey::HotKey::from_str(&self.cfg.hotkey).err().map(|e| e.to_string())
    }

    fn save(&mut self) -> Result<()> {
        if let Some(e) = self.hotkey_error() {
            return Err(anyhow!("Invalid hotkey: {e}"));
        }
        self.cfg.save()?;
        secrets::set_client(&self.client_id, &self.client_secret)?;
        Ok(())
    }

    fn poll_jobs(&mut self) {
        if let Some((rx, _)) = &self.login {
            if let Ok(res) = rx.try_recv() {
                self.login = None;
                match res {
                    Ok(t) => {
                        let name = if t.account_username.is_empty() { "(unknown)".into() } else { t.account_username };
                        self.status = Some((format!("Logged in as {name}"), false));
                        self.account = Some(name);
                    }
                    Err(e) => self.status = Some((format!("Login failed: {e:#}"), true)),
                }
            }
        }
        if let Some(rx) = &self.albums_job {
            if let Ok(res) = rx.try_recv() {
                self.albums_job = None;
                match res {
                    Ok(a) => {
                        self.status = Some((format!("Loaded {} album(s)", a.len()), false));
                        self.albums = a;
                    }
                    Err(e) => self.status = Some((format!("Couldn't load albums: {e:#}"), true)),
                }
            }
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs();
        if self.login.is_some() || self.albums_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        egui::TopBottomPanel::bottom("buttons").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.status = Some(match self.save() {
                        Ok(()) => ("Saved.".into(), false),
                        Err(e) => (format!("{e:#}"), true),
                    });
                }
                if ui.button("Save & close").clicked() {
                    match self.save() {
                        Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => self.status = Some((format!("{e:#}"), true)),
                    }
                }
                if ui.button("Cancel").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if let Some((msg, err)) = &self.status {
                    let color = if *err { egui::Color32::from_rgb(230, 80, 70) } else { ui.visuals().weak_text_color() };
                    ui.colored_label(color, msg);
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("General");
                egui::Grid::new("general").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Hotkey");
                    ui.vertical(|ui| {
                        ui.text_edit_singleline(&mut self.cfg.hotkey);
                        match self.hotkey_error() {
                            Some(e) => ui.colored_label(egui::Color32::from_rgb(230, 80, 70), e),
                            None => ui.weak("e.g. Ctrl+Shift+R, Alt+F9. Press it to start; press again to stop."),
                        };
                    });
                    ui.end_row();

                    ui.label("Output folder");
                    ui.horizontal(|ui| {
                        let mut s = self.cfg.output_dir.display().to_string();
                        if ui.add(egui::TextEdit::singleline(&mut s).desired_width(300.0)).changed() {
                            self.cfg.output_dir = s.into();
                        }
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().set_directory(&self.cfg.output_dir).pick_folder() {
                                self.cfg.output_dir = p;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Cursor");
                    ui.checkbox(&mut self.cfg.capture_cursor, "Show the mouse cursor in recordings");
                    ui.end_row();

                    ui.label("After recording");
                    egui::ComboBox::from_id_salt("default_action")
                        .selected_text(action_label(self.cfg.default_action))
                        .show_ui(ui, |ui| {
                            for a in [DefaultAction::Ask, DefaultAction::SaveMp4, DefaultAction::SaveGif, DefaultAction::Upload] {
                                ui.selectable_value(&mut self.cfg.default_action, a, action_label(a));
                            }
                        });
                    ui.end_row();
                });

                ui.add_space(12.0);
                ui.heading("Video (MP4)");
                egui::Grid::new("video").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Framerate");
                    ui.horizontal(|ui| {
                        for fps in [15, 30, 60] {
                            ui.selectable_value(&mut self.cfg.video_fps, fps, format!("{fps} fps"));
                        }
                    });
                    ui.end_row();
                    ui.label("Quality");
                    ui.vertical(|ui| {
                        ui.add(egui::Slider::new(&mut self.cfg.video_quality, 1..=100));
                        let mbps = self.cfg.video_bitrate(1920, 1080) as f64 / 1e6;
                        ui.weak(format!("≈ {mbps:.0} Mbit/s for a 1080p region"));
                    });
                    ui.end_row();
                });

                ui.add_space(12.0);
                ui.heading("GIF");
                egui::Grid::new("gif").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Framerate");
                    ui.horizontal(|ui| {
                        for fps in [10, 15, 20, 30] {
                            ui.selectable_value(&mut self.cfg.gif_fps, fps, format!("{fps} fps"));
                        }
                    });
                    ui.end_row();
                    ui.label("Max width");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.cfg.gif_max_width).range(0..=3840).suffix(" px"));
                        ui.weak("0 = original size");
                    });
                    ui.end_row();
                    ui.label("Quality");
                    ui.add(egui::Slider::new(&mut self.cfg.gif_quality, 1..=100));
                    ui.end_row();
                });

                ui.add_space(12.0);
                ui.heading("Imgur");
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Register an app at");
                    ui.hyperlink_to("api.imgur.com/oauth2/addclient", "https://api.imgur.com/oauth2/addclient");
                    ui.weak("with \"OAuth 2 authorization with a callback URL\" and callback");
                    ui.code(imgur::CALLBACK_URL);
                    if ui.small_button("Copy").clicked() {
                        ctx.copy_text(imgur::CALLBACK_URL.into());
                    }
                });
                ui.add_space(4.0);
                egui::Grid::new("imgur").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Client ID");
                    ui.add(egui::TextEdit::singleline(&mut self.client_id).desired_width(300.0));
                    ui.end_row();
                    ui.label("Client Secret");
                    ui.add(egui::TextEdit::singleline(&mut self.client_secret).password(true).desired_width(300.0));
                    ui.end_row();

                    ui.label("Account");
                    ui.horizontal(|ui| {
                        if let Some((_, cancel)) = &self.login {
                            ui.spinner();
                            ui.label("Waiting for the browser…");
                            if ui.button("Cancel").clicked() {
                                cancel.store(true, Ordering::Relaxed);
                            }
                        } else if let Some(name) = self.account.clone() {
                            ui.label(format!("Logged in as {name}"));
                            if ui.button("Log out").clicked() {
                                match imgur::logout() {
                                    Ok(()) => {
                                        self.account = None;
                                        self.albums.clear();
                                    }
                                    Err(e) => self.status = Some((format!("{e:#}"), true)),
                                }
                            }
                        } else {
                            let ready = !self.client_id.trim().is_empty() && !self.client_secret.trim().is_empty();
                            if ui.add_enabled(ready, egui::Button::new("Log in with Imgur")).clicked() {
                                match secrets::set_client(&self.client_id, &self.client_secret) {
                                    Ok(()) => {
                                        let (tx, rx) = mpsc::channel();
                                        let cancel = Arc::new(AtomicBool::new(false));
                                        let c = cancel.clone();
                                        std::thread::spawn(move || {
                                            let _ = tx.send(imgur::login(c));
                                        });
                                        self.login = Some((rx, cancel));
                                    }
                                    Err(e) => self.status = Some((format!("{e:#}"), true)),
                                }
                            }
                            if !ready {
                                ui.weak("Enter Client ID and Secret first");
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Album");
                    ui.horizontal(|ui| {
                        let current = self.cfg.imgur_album_title.clone().unwrap_or_else(|| "None (account only)".into());
                        egui::ComboBox::from_id_salt("album").selected_text(current).show_ui(ui, |ui| {
                            if ui.selectable_label(self.cfg.imgur_album_id.is_none(), "None (account only)").clicked() {
                                self.cfg.imgur_album_id = None;
                                self.cfg.imgur_album_title = None;
                            }
                            for a in &self.albums {
                                let sel = self.cfg.imgur_album_id.as_deref() == Some(a.id.as_str());
                                if ui.selectable_label(sel, &a.title).clicked() {
                                    self.cfg.imgur_album_id = Some(a.id.clone());
                                    self.cfg.imgur_album_title = Some(a.title.clone());
                                }
                            }
                        });
                        let can = self.account.is_some() && self.albums_job.is_none();
                        if ui.add_enabled(can, egui::Button::new("Load albums")).clicked() {
                            let (tx, rx) = mpsc::channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(imgur::albums());
                            });
                            self.albums_job = Some(rx);
                        }
                        if self.albums_job.is_some() {
                            ui.spinner();
                        }
                    });
                    ui.end_row();

                    ui.label("Upload as");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.cfg.upload_format, UploadFormat::Mp4, "MP4 (recommended)");
                        ui.selectable_value(&mut self.cfg.upload_format, UploadFormat::Gif, "GIF");
                    });
                    ui.end_row();

                    ui.label("Link");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut self.cfg.auto_copy_link, "Copy the link to the clipboard after upload");
                        ui.checkbox(&mut self.cfg.copy_direct_link, "Use the direct media link (i.imgur.com/…) instead of the page");
                    });
                    ui.end_row();
                });
                ui.add_space(8.0);
                ui.weak("Imgur limits videos to about 60 seconds / 200 MB. Credentials and tokens are stored in Windows Credential Manager.");
            });
        });
    }
}

pub fn action_label(a: DefaultAction) -> &'static str {
    match a {
        DefaultAction::Ask => "Ask (show the popup)",
        DefaultAction::SaveMp4 => "Save MP4",
        DefaultAction::SaveGif => "Save GIF",
        DefaultAction::Upload => "Upload to Imgur",
    }
}
