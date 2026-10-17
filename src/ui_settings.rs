//! Settings window (runs as `screen2imgur.exe settings`).

use std::str::FromStr;

use anyhow::{anyhow, Result};
use eframe::egui;

use crate::config::{Config, DefaultAction, DragFormat};

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
    status: Option<(String, bool)>,
}

impl SettingsApp {
    fn new() -> Self {
        Self {
            cfg: Config::load(),
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
        self.cfg.save()

    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                            for a in [DefaultAction::Ask, DefaultAction::SaveMp4, DefaultAction::SaveGif] {
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
                ui.heading("Drag & drop");
                egui::Grid::new("drag").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Drag out");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.cfg.drag_format, DragFormat::Mp4, "MP4");
                        ui.selectable_value(&mut self.cfg.drag_format, DragFormat::Gif, "GIF");
                    });
                    ui.end_row();
                });
                ui.weak("Dragging the preview in the popup saves the clip to the output folder and drops that file wherever you release it: the Imgur upload page, Discord, Explorer…")
            });
        });
    }
}

pub fn action_label(a: DefaultAction) -> &'static str {
    match a {
        DefaultAction::Ask => "Ask (show the popup)",
        DefaultAction::SaveMp4 => "Save MP4",
        DefaultAction::SaveGif => "Save GIF",
    }
}
