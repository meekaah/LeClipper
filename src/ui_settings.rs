//! Settings window (runs as `screen2imgur.exe settings`).
//!
//! A sidebar of tabs next to a scrolling column of cards. Every change is written to
//! config.toml shortly after it's made (debounced so sliders and typing don't hammer the
//! disk); the tray picks the new config up when this window closes.

use std::str::FromStr;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Pos2, Rect, RichText, Sense,
    Shadow, Stroke, StrokeKind, Ui,
};

use crate::config::{Config, DefaultAction, DragFormat};
use crate::imgur_web::{self, Event, Job, Task};

pub fn app_icon() -> egui::IconData {
    egui::IconData { rgba: crate::icon::rgba(false), width: crate::icon::SIZE, height: crate::icon::SIZE }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screen2imgur settings")
            .with_inner_size([900.0, 760.0])
            .with_min_inner_size([680.0, 460.0])
            .with_icon(app_icon()),
        centered: true,
        ..Default::default()
    };
    eframe::run_native("screen2imgur settings", options, Box::new(|cc| Ok(Box::new(SettingsApp::new(cc)))))
        .map_err(|e| anyhow!("{e}"))
}

// Palette.
const SIDEBAR: Color32 = Color32::from_rgb(18, 20, 25);
const BG: Color32 = Color32::from_rgb(24, 27, 33);
const CARD: Color32 = Color32::from_rgb(32, 36, 44);
const CARD_BORDER: Color32 = Color32::from_rgb(44, 49, 59);
const FIELD: Color32 = Color32::from_rgb(21, 24, 29);
const FIELD_BORDER: Color32 = Color32::from_rgb(53, 59, 71);
const HOVER: Color32 = Color32::from_rgb(42, 47, 57);
const TEXT: Color32 = Color32::from_rgb(231, 233, 238);
const WEAK: Color32 = Color32::from_rgb(139, 147, 161);
const ACCENT: Color32 = Color32::from_rgb(79, 140, 255);
const GREEN: Color32 = Color32::from_rgb(63, 185, 80);
const RED: Color32 = Color32::from_rgb(242, 85, 90);
const AMBER: Color32 = Color32::from_rgb(227, 163, 59);

// Segoe Fluent Icons / Segoe MDL2 Assets code points.
const ICON_GENERAL: &str = "\u{E713}";
const ICON_VIDEO: &str = "\u{E714}";
const ICON_GLOBE: &str = "\u{E774}";
const ICON_KEYBOARD: &str = "\u{E765}";
const ICON_RECORD: &str = "\u{E7C8}";
const ICON_FOLDER: &str = "\u{E8B7}";
const ICON_PICTURE: &str = "\u{E8B9}";
const ICON_CHECK: &str = "\u{E73E}";
const ICON_ERROR: &str = "\u{E783}";
const ICON_MINUS: &str = "\u{E738}";
const ICON_PLUS: &str = "\u{E710}";

const TOAST_TIME: f32 = 2.4;
const SAVE_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    General,
    Video,
    Integrations,
}

#[derive(Clone, Copy, PartialEq)]
enum ImgurOp {
    Check,
    SignIn,
    SignOut,
}

#[derive(Clone, Copy, PartialEq)]
enum Auth {
    Unknown,
    SignedIn,
    SignedOut,
}

struct SettingsApp {
    cfg: Config,
    /// What's on disk, and the config as of the last frame (to debounce saves).
    saved: Config,
    last_seen: Config,
    last_change: Instant,
    tab: Tab,
    logo: egui::TextureHandle,
    imgur_logo: egui::TextureHandle,
    listening: bool,
    hotkey_reject: Option<String>,
    toast: Option<(String, bool, Instant)>,
    /// A running Imgur job and what it last reported.
    imgur: Option<(ImgurOp, Job, Receiver<Event>)>,
    imgur_status: Option<(String, bool)>,
    auth: Auth,
    username: Option<String>,
}

impl SettingsApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        style_window(cc);
        install_fonts(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let size = crate::icon::SIZE as usize;
        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([size, size], &crate::icon::rgba(false)),
            Default::default(),
        );
        let side = crate::icon::IMGUR_SIZE;
        let imgur_logo = cc.egui_ctx.load_texture(
            "imgur-logo",
            egui::ColorImage::from_rgba_unmultiplied([side, side], crate::icon::IMGUR_RGBA),
            Default::default(),
        );
        let cfg = Config::load();
        let mut app = Self {
            saved: cfg.clone(),
            last_seen: cfg.clone(),
            cfg,
            last_change: Instant::now(),
            tab: Tab::General,
            logo,
            imgur_logo,
            listening: false,
            hotkey_reject: None,
            toast: None,
            imgur: None,
            imgur_status: None,
            auth: Auth::Unknown,
            username: None,
        };
        app.imgur_task(&cc.egui_ctx, ImgurOp::Check);
        app
    }

    fn imgur_task(&mut self, ctx: &egui::Context, op: ImgurOp) {
        let task = match op {
            ImgurOp::Check => Task::Status,
            ImgurOp::SignIn => Task::SignIn,
            ImgurOp::SignOut => Task::SignOut,
        };
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        let job = imgur_web::start(task, tx, move || wake.request_repaint());
        if op != ImgurOp::Check {
            self.imgur_status = None;
        }
        self.imgur = Some((op, job, rx));
    }

    fn poll_imgur(&mut self) {
        let Some((op, _, rx)) = &self.imgur else { return };
        let op = *op;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Status(s) => {
                    if op != ImgurOp::Check {
                        self.imgur_status = Some((s, false));
                    }
                }
                Event::Progress(_) => {}
                Event::Account(name) => {
                    self.auth = Auth::SignedIn;
                    if name.is_some() {
                        self.username = name;
                    }
                }
                Event::Done(s) => {
                    self.auth = match op {
                        ImgurOp::Check if s == imgur_web::SIGNED_IN => Auth::SignedIn,
                        ImgurOp::Check => Auth::SignedOut,
                        ImgurOp::SignIn => Auth::SignedIn,
                        ImgurOp::SignOut => Auth::SignedOut,
                    };
                    if self.auth == Auth::SignedOut {
                        self.username = None;
                    }
                    self.imgur_status = (op != ImgurOp::Check).then_some((s, false));
                    self.imgur = None;
                    return;
                }
                Event::Failed(e) => {
                    self.imgur_status = Some(match op {
                        ImgurOp::Check => (format!("Couldn't check the Imgur session: {e}"), true),
                        _ if e == "cancelled" => ("Cancelled.".into(), false),
                        _ => (e, true),
                    });
                    self.imgur = None;
                    return;
                }
            }
        }
    }

    fn hotkey_error(&self) -> Option<String> {
        global_hotkey::hotkey::HotKey::from_str(&self.cfg.hotkey).err().map(|e| e.to_string())
    }

    fn toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, Instant::now()));
    }

    /// Writes the config once it has stopped changing for a moment.
    fn autosave(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.cfg != self.last_seen {
            self.last_seen = self.cfg.clone();
            self.last_change = now;
        }
        if self.cfg == self.saved {
            return;
        }
        let dragging = ctx.input(|i| i.pointer.any_down());
        if dragging || now - self.last_change < SAVE_DELAY {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        // Never write a hotkey the tray couldn't register; the capture only sets valid ones.
        if self.hotkey_error().is_some() {
            self.cfg.hotkey = self.saved.hotkey.clone();
        }
        match self.cfg.save() {
            Ok(()) => self.toast("Changes saved automatically", false),
            Err(e) => self.toast(format!("Couldn't save settings: {e:#}"), true),
        }
        self.saved = self.cfg.clone();
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(22.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(30.0, 30.0)));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("screen2imgur").family(semibold()).size(15.0).color(TEXT));
                ui.label(RichText::new("settings").size(12.0).color(WEAK));
            });
        });
        ui.add_space(26.0);
        for (tab, icon, label) in [
            (Tab::General, ICON_GENERAL, "General"),
            (Tab::Video, ICON_VIDEO, "Video & GIF"),
            (Tab::Integrations, ICON_GLOBE, "Integrations"),
        ] {
            if nav_item(ui, icon, label, self.tab == tab).clicked() {
                self.tab = tab;
                self.listening = false;
            }
            ui.add_space(2.0);
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).size(12.0).color(WEAK));
            });
        });
    }

    fn general(&mut self, ui: &mut Ui) {
        page_header(ui, "General", "Core behavior, output and workflow.");

        card(ui, ICON_RECORD, "Recording & input", |ui| {
            row(ui, "Global hotkey", "Press once to pick a region, again to stop.", |ui| self.hotkey_pill(ui));
            if let Some(msg) = self.hotkey_reject.clone().or_else(|| self.hotkey_error()) {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    ui.label(RichText::new(msg).size(12.0).color(RED));
                });
            }
            divider(ui);
            row(ui, "Show mouse pointer in recordings", "Draws the cursor into the captured video.", |ui| {
                toggle(ui, &mut self.cfg.capture_cursor);
            });
        });

        card(ui, ICON_FOLDER, "Output & workflow", |ui| {
            label_block(ui, "Output folder", "Where saved MP4s and GIFs go.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, "Browse…", Kind::Outline, true).clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_directory(&self.cfg.output_dir).pick_folder() {
                            self.cfg.output_dir = p;
                        }
                    }
                    let mut s = self.cfg.output_dir.display().to_string();
                    let edit = egui::TextEdit::singleline(&mut s)
                        .desired_width(ui.available_width() - 4.0)
                        .margin(Margin::symmetric(10, 8));
                    if ui.add(edit).changed() {
                        self.cfg.output_dir = s.into();
                    }
                });
            });
            divider(ui);
            row(ui, "After recording", "What happens when you stop recording.", |ui| {
                egui::ComboBox::from_id_salt("default_action")
                    .width(200.0)
                    .height(240.0)
                    .selected_text(action_label(self.cfg.default_action))
                    .show_ui(ui, |ui| {
                        for a in [DefaultAction::Ask, DefaultAction::SaveMp4, DefaultAction::SaveGif, DefaultAction::Upload] {
                            ui.selectable_value(&mut self.cfg.default_action, a, action_label(a));
                        }
                    });
            });
            divider(ui);
            row(ui, "Drag out format", "What gets dragged out of the preview popup after recording.", |ui| {
                segmented(ui, "drag_format", &mut self.cfg.drag_format, &[(DragFormat::Mp4, "MP4"), (DragFormat::Gif, "GIF")]);
            });
            helper(
                ui,
                "The clip is saved to the output folder in this format and dropped wherever you let go (Discord, Explorer, etc.).",
            );
        });
    }

    fn video(&mut self, ui: &mut Ui) {
        page_header(ui, "Video & GIF", "Encoding settings for each output format.");

        card(ui, ICON_VIDEO, "MP4 encoding", |ui| {
            row(ui, "Framerate", "Frames captured per second.", |ui| {
                let opts = [15, 30, 60].map(|f| (f, format!("{f} fps")));
                segmented(ui, "video_fps", &mut self.cfg.video_fps, &opts);
            });
            divider(ui);
            row(ui, "Quality", "Higher means a bigger, sharper file.", |ui| {
                slider(ui, &mut self.cfg.video_quality);
            });
            let mbps = self.cfg.video_bitrate(1920, 1080) as f64 / 1e6;
            helper(
                ui,
                &format!(
                    "≈ {mbps:.0} Mbit/s for a 1080p region. The bitrate scales between 1 and 120 Mbit/s with region size and framerate."
                ),
            );
        });

        card(ui, ICON_PICTURE, "GIF encoding", |ui| {
            row(ui, "Framerate", "Lower framerates make smaller GIFs.", |ui| {
                let opts = [10, 15, 20, 30].map(|f| (f, format!("{f} fps")));
                segmented(ui, "gif_fps", &mut self.cfg.gif_fps, &opts);
            });
            divider(ui);
            row(ui, "Max width", "0 = original size", |ui| stepper(ui, &mut self.cfg.gif_max_width));
            divider(ui);
            row(ui, "Quality", "gifski quality: color accuracy and dithering.", |ui| {
                let mut q = self.cfg.gif_quality as u32;
                slider(ui, &mut q);
                self.cfg.gif_quality = q as u8;
            });
        });
    }

    fn integrations(&mut self, ui: &mut Ui) {
        page_header(ui, "Integrations", "Connected services and sessions.");
        let ctx = ui.ctx().clone();

        let imgur_logo = self.imgur_logo.clone();
        image_card(ui, &imgur_logo, "Imgur", |ui| {
            let op = self.imgur.as_ref().map(|(op, ..)| *op);
            let checking = op == Some(ImgurOp::Check);
            let (dot, title, sub) = match self.auth {
                Auth::SignedIn => (
                    GREEN,
                    match &self.username {
                        Some(name) => format!("Signed in as {name}"),
                        None => "Signed in".into(),
                    },
                    if checking { "Looking up your username…" } else { "Your imgur.com session is active." },
                ),
                _ if checking => (WEAK, "Checking session…".into(), "Looking for a saved imgur.com sign-in."),
                Auth::SignedOut => (RED, "Signed out".into(), "Sign in to upload to your Imgur account."),
                Auth::Unknown => (WEAK, "Status unknown".into(), "The session couldn't be checked."),
            };
            ui.horizontal(|ui| {
                ui.set_min_height(44.0);
                status_dot(ui, dot, dot != WEAK);
                ui.add_space(4.0);
                label_block(ui, &title, sub);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| match op {
                    Some(ImgurOp::SignIn | ImgurOp::SignOut) => {
                        if button(ui, "Cancel", Kind::Warning, true).clicked() {
                            if let Some((_, job, _)) = &self.imgur {
                                job.cancel();
                            }
                        }
                        ui.add_space(4.0);
                        ui.spinner();
                    }
                    Some(ImgurOp::Check) => {
                        ui.spinner();
                    }
                    None if self.auth == Auth::SignedIn => {
                        if button(ui, "Sign out", Kind::Outline, true).clicked() {
                            self.imgur_task(&ctx, ImgurOp::SignOut);
                        }
                    }
                    None => {
                        if button(ui, "Sign in…", Kind::Accent, true).clicked() {
                            self.imgur_task(&ctx, ImgurOp::SignIn);
                        }
                    }
                });
            });
            if let Some((msg, err)) = &self.imgur_status {
                ui.add_space(6.0);
                ui.label(RichText::new(msg).size(12.0).color(if *err { RED } else { WEAK }));
            }
        });
    }

    /// The hotkey button; while listening, the next valid combination replaces the hotkey.
    fn hotkey_pill(&mut self, ui: &mut Ui) {
        let text = if self.listening { "Listening for keys…".to_owned() } else { self.cfg.hotkey.clone() };
        let font = FontId::proportional(14.0);
        let galley = ui.painter().layout_no_wrap(text, font, if self.listening { ACCENT } else { TEXT });
        let w = (galley.size().x + 16.0 + 8.0 + 36.0).max(180.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() || self.listening, 0.15);
        let p = ui.painter();
        let r = CornerRadius::same(17);
        if self.listening {
            let t = ui.input(|i| i.time) as f32;
            let pulse = (t * 4.0).sin() * 0.5 + 0.5;
            p.rect_stroke(rect.expand(2.0 + 2.0 * pulse), 19, Stroke::new(2.0, ACCENT.gamma_multiply(0.25 + 0.35 * pulse)), StrokeKind::Outside);
            ui.ctx().request_repaint();
        }
        p.rect(
            rect,
            r,
            mix(FIELD, HOVER, hover),
            Stroke::new(1.0, if self.listening { ACCENT } else { mix(FIELD_BORDER, ACCENT, hover * 0.5) }),
            StrokeKind::Inside,
        );
        let content_w = 16.0 + 8.0 + galley.size().x;
        let x = rect.center().x - content_w / 2.0;
        p.text(
            Pos2::new(x, rect.center().y),
            Align2::LEFT_CENTER,
            ICON_KEYBOARD,
            FontId::new(16.0, icons()),
            if self.listening { ACCENT } else { WEAK },
        );
        let y = centered_galley_pos(&galley, rect.center()).y;
        p.galley(Pos2::new(x + 24.0, y), galley, TEXT);

        if resp.clicked() {
            self.listening = !self.listening;
            self.hotkey_reject = None;
            return;
        }
        if !self.listening {
            return;
        }
        if ui.input(|i| i.pointer.any_pressed()) && !resp.hovered() {
            self.listening = false;
            return;
        }
        let keys: Vec<_> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, physical_key, pressed: true, repeat: false, modifiers } => {
                        Some((physical_key.unwrap_or(*key), *modifiers))
                    }
                    _ => None,
                })
                .collect()
        });
        for (key, mods) in keys {
            if key == egui::Key::Escape && !mods.any() {
                self.listening = false;
                return;
            }
            match hotkey_from(key, mods) {
                Ok(s) => {
                    self.cfg.hotkey = s;
                    self.hotkey_reject = None;
                    self.listening = false;
                    return;
                }
                Err(e) => self.hotkey_reject = Some(e),
            }
        }
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, err, at)) = &self.toast else { return };
        let t = at.elapsed().as_secs_f32();
        if t > TOAST_TIME {
            self.toast = None;
            return;
        }
        let fade = (t / 0.15).min(1.0).min((TOAST_TIME - t) / 0.4).clamp(0.0, 1.0);
        let lift = (1.0 - (t / 0.2).min(1.0)) * 8.0;
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0 + lift))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(fade);
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0, FIELD_BORDER))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(12, 8))
                    .shadow(Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(110) })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (icon, color) = if *err { (ICON_ERROR, RED) } else { (ICON_CHECK, GREEN) };
                            ui.label(RichText::new(icon).family(icons()).size(12.0).color(color));
                            ui.label(RichText::new(msg.as_str()).size(12.0).color(TEXT));
                        });
                    });
            });
        ctx.request_repaint();
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_imgur();

        egui::SidePanel::left("nav")
            .exact_width(220.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin::symmetric(12, 0)))
            .show(ctx, |ui| self.sidebar(ui));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG)).show(ctx, |ui| {
            // A hairline between the sidebar and the content.
            let r = ui.max_rect();
            ui.painter().vline(r.left(), r.y_range(), Stroke::new(1.0, CARD_BORDER));
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin { left: 32, right: 32, top: 28, bottom: 32 }).show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(760.0));
                    match self.tab {
                        Tab::General => self.general(ui),
                        Tab::Video => self.video(ui),
                        Tab::Integrations => self.integrations(ui),
                    }
                });
            });
        });

        self.autosave(ctx);
        self.toast_ui(ctx);
    }
}

impl Drop for SettingsApp {
    fn drop(&mut self) {
        if self.cfg != self.saved && self.hotkey_error().is_none() {
            let _ = self.cfg.save();
        }
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

/// Turns a key press into a hotkey string global-hotkey accepts, or says why it can't be one.
fn hotkey_from(key: egui::Key, mods: egui::Modifiers) -> Result<String, String> {
    use egui::Key::*;
    let fkey = matches!(
        key,
        F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 | F13 | F14 | F15 | F16 | F17 | F18 | F19 | F20
    );
    if !(mods.ctrl || mods.alt || fkey) {
        return Err("Use Ctrl or Alt with the key (F-keys work on their own).".into());
    }
    let name = match key {
        Backtick => "Backquote",
        OpenBracket => "BracketLeft",
        CloseBracket => "BracketRight",
        Equals | Plus => "Equal",
        k => k.name(),
    };
    let mut s = String::new();
    for (on, m) in [(mods.ctrl, "Ctrl"), (mods.alt, "Alt"), (mods.shift, "Shift")] {
        if on {
            s.push_str(m);
            s.push('+');
        }
    }
    s.push_str(name);
    match global_hotkey::hotkey::HotKey::from_str(&s) {
        Ok(_) => Ok(s),
        Err(_) => Err(format!("{s} can't be used as a hotkey.")),
    }
}

// ---- Look ----

fn icons() -> FontFamily {
    FontFamily::Name("icons".into())
}

fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// Segoe UI for text and the system icon font for glyphs, when Windows has them.
fn install_fonts(ctx: &egui::Context) {
    let dir = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let load = |names: &[&str]| names.iter().find_map(|n| std::fs::read(dir.join(n)).ok());
    let mut fonts = egui::FontDefinitions::default();
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    let mut family = |name: &str, files: &[&str], family: FontFamily| {
        let mut chain = fallback.clone();
        if let Some(data) = load(files) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(data)));
            chain.insert(0, name.into());
        }
        fonts.families.insert(family, chain);
    };
    family("segoe", &["segoeui.ttf"], FontFamily::Proportional);
    family("segoe-sb", &["seguisb.ttf", "segoeui.ttf"], semibold());
    family("icons", &["SegoeIcons.ttf", "segmdl2.ttf"], icons());
    ctx.set_fonts(fonts);
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = FIELD;
        v.faint_bg_color = CARD;
        v.window_stroke = Stroke::new(1.0, FIELD_BORDER);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        let shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = ACCENT;
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT);

        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, CARD_BORDER);
        w.inactive.weak_bg_fill = FIELD;
        w.inactive.bg_fill = FIELD_BORDER;
        w.inactive.bg_stroke = Stroke::new(1.0, FIELD_BORDER);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_fill = Color32::from_rgb(112, 160, 255);
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
        w.active.weak_bg_fill = HOVER;
        w.active.bg_fill = ACCENT;
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.0, TEXT);
        w.open.weak_bg_fill = HOVER;
        w.open.bg_stroke = Stroke::new(1.0, ACCENT);
        w.open.fg_stroke = Stroke::new(1.0, TEXT);
        for w in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            w.corner_radius = CornerRadius::same(8);
            w.expansion = 0.0;
        }

        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 7.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.slider_width = 220.0;
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.animation_time = 0.15;
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(14.0));
        s.text_styles.insert(Button, FontId::proportional(14.0));
        s.text_styles.insert(Small, FontId::proportional(12.0));
        s.text_styles.insert(Heading, FontId::new(22.0, semibold()));
    });
}

/// Dark title bar in the sidebar's color, with a faint accent border (Windows 11).
fn style_window(cc: &eframe::CreationContext) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::*;
    let Ok(handle) = cc.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    let colorref = |c: Color32| c.r() as u32 | (c.g() as u32) << 8 | (c.b() as u32) << 16;
    let set = |attr: DWMWINDOWATTRIBUTE, value: u32| unsafe {
        let _ = DwmSetWindowAttribute(hwnd, attr, &value as *const u32 as *const _, 4);
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0 as u32);
    set(DWMWA_CAPTION_COLOR, colorref(SIDEBAR));
    set(DWMWA_TEXT_COLOR, colorref(TEXT));
    set(DWMWA_BORDER_COLOR, colorref(Color32::from_rgb(52, 72, 118)));
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

// ---- Layout pieces ----

fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).family(semibold()).size(24.0).color(TEXT));
    ui.label(RichText::new(subtitle).size(13.0).color(WEAK));
    ui.add_space(18.0);
}

fn card(ui: &mut Ui, icon: &str, title: &str, body: impl FnOnce(&mut Ui)) {
    card_with(ui, title, body, |ui, rect| {
        ui.painter().rect_filled(rect, 7, ACCENT.gamma_multiply(0.16));
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::new(14.0, icons()), ACCENT);
    });
}

/// A card whose header shows a full-colour image instead of a tinted glyph.
fn image_card(ui: &mut Ui, image: &egui::TextureHandle, title: &str, body: impl FnOnce(&mut Ui)) {
    card_with(ui, title, body, |ui, rect| {
        egui::Image::new(image).corner_radius(7).paint_at(ui, rect);
    });
}

fn card_with(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui), icon: impl FnOnce(&Ui, Rect)) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(20))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                icon(ui, rect);
                ui.add_space(4.0);
                ui.label(RichText::new(title).family(semibold()).size(15.0).color(TEXT));
            });
            ui.add_space(14.0);
            body(ui);
        });
    ui.add_space(16.0);
}

fn label_block(ui: &mut Ui, title: &str, desc: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(title).size(14.0).color(TEXT));
        if !desc.is_empty() {
            ui.label(RichText::new(desc).size(12.0).color(WEAK));
        }
    });
}

/// A setting: title and description on the left, its control on the right.
fn row(ui: &mut Ui, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(40.0);
        label_block(ui, title, desc);
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

fn helper(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(WEAK)).wrap());
}

fn divider(ui: &mut Ui) {
    ui.add_space(10.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, CARD_BORDER);
    ui.add_space(10.0);
}

// ---- Widgets ----

fn nav_item(ui: &mut Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let sel = ui.ctx().animate_bool_with_time(resp.id.with("sel"), selected, 0.15);
    let p = ui.painter();
    let bg = mix(mix(Color32::TRANSPARENT, Color32::from_rgb(30, 33, 41), hover), Color32::from_rgb(36, 41, 52), sel);
    p.rect_filled(rect, 8, bg);
    if sel > 0.0 {
        let h = 18.0 * sel;
        let bar = Rect::from_center_size(Pos2::new(rect.left() + 1.5, rect.center().y), vec2(3.0, h));
        p.rect_filled(bar, 2, ACCENT);
    }
    let fg = mix(WEAK, TEXT, sel.max(hover));
    p.text(Pos2::new(rect.left() + 16.0, rect.center().y), Align2::LEFT_CENTER, icon, FontId::new(15.0, icons()), mix(fg, ACCENT, sel));
    p.text(Pos2::new(rect.left() + 44.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.0), fg);
    resp
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Accent,
    Outline,
    Warning,
}

/// Where to paint a one-line galley so its visible glyphs, cap top to baseline, sit centered on `center`.
/// Centering the galley's box puts Segoe UI text low, because its line box has more room above the caps
/// than below the baseline.
fn centered_galley_pos(galley: &egui::Galley, center: Pos2) -> Pos2 {
    let ink = galley.rows.first().and_then(|row| {
        let glyphs = row.glyphs.iter().filter(|g| !g.uv_rect.is_nothing());
        let top = glyphs.map(|g| g.pos.y + g.uv_rect.offset.y).reduce(f32::min)?;
        let baseline = row.glyphs.first()?.pos.y;
        Some((row.pos.y + top, row.pos.y + baseline))
    });
    let mid_y = ink.map_or(galley.size().y / 2.0, |(top, baseline)| (top + baseline) / 2.0);
    Pos2::new(center.x - galley.size().x / 2.0, (center.y - mid_y).round())
}

/// A button that lifts and casts a soft shadow on hover.
fn button(ui: &mut Ui, text: &str, kind: Kind, enabled: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(14.0), TEXT);
    let size = vec2(galley.size().x + 32.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.15);
    let pressed = enabled && resp.is_pointer_button_down_on();
    let rect = rect.translate(vec2(0.0, if pressed { 0.0 } else { -hover }));
    let p = ui.painter();
    let (fill, stroke, fg) = match kind {
        Kind::Accent => (mix(ACCENT, Color32::from_rgb(110, 162, 255), hover), Color32::TRANSPARENT, Color32::WHITE),
        Kind::Outline => (mix(Color32::TRANSPARENT, HOVER, hover), mix(FIELD_BORDER, WEAK, hover * 0.6), TEXT),
        Kind::Warning => (mix(AMBER.gamma_multiply(0.10), AMBER.gamma_multiply(0.2), hover), AMBER.gamma_multiply(0.8), AMBER),
    };
    let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.4) };
    if hover > 0.0 {
        let shadow = Shadow { offset: [0, 3], blur: (10.0 * hover) as u8, spread: 0, color: Color32::from_black_alpha((90.0 * hover) as u8) };
        p.add(shadow.as_shape(rect, CornerRadius::same(8)));
    }
    p.rect(rect, 8, dim(fill), Stroke::new(1.0, dim(stroke)), StrokeKind::Inside);
    p.galley(centered_galley_pos(&galley, rect.center()), galley, dim(fg));
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// iOS-style switch with a sliding knob.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(44.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
    let p = ui.painter();
    let track = mix(mix(FIELD_BORDER, HOVER, hover * 0.5), ACCENT, t);
    p.rect_filled(rect, 12, track);
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    p.circle_filled(Pos2::new(x, rect.center().y + 1.0), 9.0, Color32::from_black_alpha(50));
    p.circle_filled(Pos2::new(x, rect.center().y), 9.0 + hover * 0.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Pill-shaped segmented control; the highlight slides to the selected option.
fn segmented<T: PartialEq + Copy, S: AsRef<str>>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, S)]) -> bool {
    let font = FontId::proportional(13.5);
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, s)| ui.painter().layout_no_wrap(s.as_ref().to_owned(), font.clone(), TEXT))
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 28.0).collect();
    let pad = 3.0;
    let (rect, _) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>() + pad * 2.0, 34.0), Sense::hover());
    let id = ui.id().with(id);
    let p = ui.painter().clone();
    p.rect(rect, 17, FIELD, Stroke::new(1.0, FIELD_BORDER), StrokeKind::Inside);

    let mut x = rect.left() + pad;
    let mut segs = Vec::new();
    for w in &widths {
        segs.push(Rect::from_min_size(Pos2::new(x, rect.top() + pad), vec2(*w, rect.height() - pad * 2.0)));
        x += w;
    }
    let mut changed = false;
    for (i, seg) in segs.iter().enumerate() {
        let r = ui.interact(*seg, id.with(i), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        if r.hovered() && options[i].0 != *value {
            p.rect_filled(seg.shrink(1.0), 14, HOVER);
        }
        if r.clicked() && options[i].0 != *value {
            *value = options[i].0;
            changed = true;
        }
    }
    if let Some(sel) = options.iter().position(|(v, _)| v == value) {
        let left = ui.ctx().animate_value_with_time(id.with("x"), segs[sel].left(), 0.15);
        let width = ui.ctx().animate_value_with_time(id.with("w"), segs[sel].width(), 0.15);
        let hl = Rect::from_min_size(Pos2::new(left, segs[sel].top()), vec2(width, segs[sel].height()));
        p.rect_filled(hl, 14, ACCENT);
    }
    for (i, (seg, g)) in segs.iter().zip(galleys).enumerate() {
        let color = if options[i].0 == *value { Color32::WHITE } else { WEAK };
        p.galley(seg.center() - g.size() / 2.0, g, color);
    }
    changed
}

/// 1..=100 slider with its value in a badge.
fn slider(ui: &mut Ui, value: &mut u32) {
    let badge = format!("{value}");
    let (rect, _) = ui.allocate_exact_size(vec2(44.0, 26.0), Sense::hover());
    ui.painter().rect_filled(rect, 13, ACCENT.gamma_multiply(0.16));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, badge, FontId::new(13.0, semibold()), ACCENT);
    ui.add_space(6.0);
    ui.add(egui::Slider::new(value, 1..=100).show_value(false).trailing_fill(true));
}

/// Number box with − / + buttons, for pixel widths.
fn stepper(ui: &mut Ui, value: &mut u32) {
    egui::Frame::new()
        .fill(FIELD)
        .stroke(Stroke::new(1.0, FIELD_BORDER))
        .corner_radius(17)
        .inner_margin(Margin::symmetric(3, 0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            // Right-to-left parent: add in reverse.
            if step_button(ui, ICON_PLUS).clicked() {
                *value = (*value / 32 * 32 + 32).min(3840);
            }
            ui.add_sized(
                vec2(84.0, 32.0),
                egui::DragValue::new(value).range(0..=3840).speed(4.0).suffix(" px"),
            );
            if step_button(ui, ICON_MINUS).clicked() {
                *value = value.saturating_sub(1) / 32 * 32;
            }
        });
}

fn step_button(ui: &mut Ui, icon: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    ui.painter().circle_filled(rect.center(), 13.0, mix(Color32::TRANSPARENT, HOVER, hover));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::new(11.0, icons()), mix(WEAK, TEXT, hover));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn status_dot(ui: &mut Ui, color: Color32, glow: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
    if glow {
        ui.painter().circle_filled(rect.center(), 8.0, color.gamma_multiply(0.22));
    }
    ui.painter().circle_filled(rect.center(), 4.5, color);
}
