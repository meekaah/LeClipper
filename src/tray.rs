//! The background process: tray icon, global hotkey, region selection and recording.
//! Settings and the post-recording popup run as child processes of the same exe.

use std::cell::RefCell;
use std::str::FromStr;

use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::{self, Recording, RecordingResult, Region};
use crate::config::Config;
use crate::overlay::{self, ArmedUi, RecordingUi, WM_APP_ARM_CANCEL, WM_APP_ARM_START, WM_APP_SELECTED, WM_APP_STOP};

const WM_APP_TRAY: u32 = WM_APP + 10;
const WM_APP_HOTKEY: u32 = WM_APP + 11;
const WM_APP_MENU: u32 = WM_APP + 12;
/// lParam = `Box<Result<RecordingResult>>`.
const WM_APP_REC_DONE: u32 = WM_APP + 13;
const WM_APP_RELOAD: u32 = WM_APP + 14;

enum State {
    Idle,
    Selecting(HWND),
    /// Region chosen, waiting for the user to press Record.
    Armed(ArmedUi),
    Recording { rec: Recording, ui: RecordingUi },
    Stopping,
}

struct Menus {
    record: MenuItem,
    reselect: MenuId,
    settings: MenuId,
    folder: MenuId,
    quit: MenuId,
}

struct App {
    hwnd: HWND,
    cfg: Config,
    tray: TrayIcon,
    menus: Menus,
    hotkeys: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    state: State,
    settings_open: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Messages that arrived while `APP` was checked out (e.g. during a modal message box).
    static DEFERRED: RefCell<Vec<(u32, WPARAM, LPARAM)>> = const { RefCell::new(Vec::new()) };
}

fn icon(recording: bool) -> Icon {
    Icon::from_rgba(crate::icon::rgba(recording), crate::icon::SIZE, crate::icon::SIZE).expect("icon")
}

fn message_box(text: &str, error: bool) {
    let t = HSTRING::from(text);
    let flags = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), w!("screen2imgur"), flags | MB_OK | MB_SETFOREGROUND);
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, true, w!("Local\\screen2imgur-tray"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box("screen2imgur is already running (look for it in the tray).", false);
            return Ok(());
        }

        clean_stale_temp_files();

        let class = w!("s2i_main");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_proc),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("screen2imgur"),
            WINDOW_STYLE::default(),
            0, 0, 0, 0,
            Some(HWND_MESSAGE), None, None, None,
        )?;
        let raw = hwnd.0 as isize;

        // Library callbacks fire on this thread inside the message loop; bounce them through
        // our window so they're handled outside any library borrow.
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if matches!(e, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) {
                let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_TRAY, WPARAM(0), LPARAM(0));
            }
        }));
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_HOTKEY, WPARAM(e.id as usize), LPARAM(0));
            }
        }));
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let id = Box::into_raw(Box::new(e.id)) as isize;
            let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_MENU, WPARAM(0), LPARAM(id));
        }));

        let cfg = Config::load();
        let record = MenuItem::new("Record region", true, None);
        let reselect = MenuItem::new("Select new region", true, None);
        let settings = MenuItem::new("Settings…", true, None);
        let folder = MenuItem::new("Open output folder", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&record, &reselect, &PredefinedMenuItem::separator(), &settings, &folder, &PredefinedMenuItem::separator(), &quit])?;

        let tray = TrayIconBuilder::new()
            .with_icon(icon(false))
            .with_tooltip("screen2imgur")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .context("creating tray icon")?;

        let hotkeys = GlobalHotKeyManager::new().context("hotkey manager")?;
        let mut app = App {
            hwnd,
            cfg,
            tray,
            menus: Menus { record, reselect: reselect.id().clone(), settings: settings.id().clone(), folder: folder.id().clone(), quit: quit.id().clone() },
            hotkeys,
            hotkey: None,
            state: State::Idle,
            settings_open: false,
        };
        app.register_hotkey();
        APP.with(|a| *a.borrow_mut() = Some(app));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|a| a.borrow_mut().take());
    }
    Ok(())
}

/// Removes recordings left behind by popups that were killed (older than a day).
fn clean_stale_temp_files() {
    let Ok(entries) = std::fs::read_dir(crate::config::temp_dir()) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 24 * 3600);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_TRAY | WM_APP_HOTKEY | WM_APP_MENU | WM_APP_SELECTED | WM_APP_ARM_START | WM_APP_ARM_CANCEL | WM_APP_STOP
        | WM_APP_REC_DONE | WM_APP_RELOAD => {
            // Take the app out while handling, so re-entrant messages (modal message boxes) don't double-borrow.
            let Some(mut app) = APP.with(|a| a.borrow_mut().take()) else {
                DEFERRED.with(|d| d.borrow_mut().push((msg, wp, lp)));
                return LRESULT(0);
            };
            let mut quit = app.handle(msg, wp, lp);
            while let Some((m, w, l)) = DEFERRED.with(|d| { let mut d = d.borrow_mut(); (!d.is_empty()).then(|| d.remove(0)) }) {
                quit |= app.handle(m, w, l);
            }
            APP.with(|a| *a.borrow_mut() = Some(app));
            if quit {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

impl App {
    fn register_hotkey(&mut self) {
        if let Some(old) = self.hotkey.take() {
            let _ = self.hotkeys.unregister(old);
        }
        match HotKey::from_str(&self.cfg.hotkey) {
            Ok(hk) => match self.hotkeys.register(hk) {
                Ok(()) => self.hotkey = Some(hk),
                Err(e) => message_box(&format!("Couldn't register hotkey {}: {e}\nPick another one in Settings.", self.cfg.hotkey), true),
            },
            Err(e) => message_box(&format!("Invalid hotkey \"{}\": {e}", self.cfg.hotkey), true),
        }
        if matches!(self.state, State::Idle) {
            self.set_record_label();
        }
    }

    fn set_record_label(&self) {
        self.menus.record.set_text(match &self.hotkey {
            Some(_) => format!("Record region\t{}", self.cfg.hotkey),
            None => "Record region".into(),
        });
    }

    /// Returns true to quit.
    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> bool {
        match msg {
            WM_APP_TRAY => {
                self.toggle();
            }
            WM_APP_HOTKEY => {
                if self.hotkey.is_some_and(|h| h.id() == wp.0 as u32) {
                    self.toggle();
                }
            }
            WM_APP_MENU => {
                let id = *unsafe { Box::from_raw(lp.0 as *mut MenuId) };
                if id == *self.menus.record.id() {
                    self.toggle();
                } else if id == self.menus.reselect {
                    self.reselect();
                } else if id == self.menus.settings {
                    self.open_settings();
                } else if id == self.menus.folder {
                    let _ = std::fs::create_dir_all(&self.cfg.output_dir);
                    let _ = open::that(&self.cfg.output_dir);
                } else if id == self.menus.quit {
                    if let State::Recording { .. } = self.state {
                        self.stop();
                    }
                    return true;
                }
            }
            WM_APP_SELECTED => {
                let region = (lp.0 != 0).then(|| *unsafe { Box::from_raw(lp.0 as *mut Region) });
                if let State::Selecting(_) = self.state {
                    self.state = State::Idle;
                    if let Some(r) = region {
                        self.arm(r);
                    }
                }
            }
            WM_APP_ARM_START => self.start_armed(),
            WM_APP_ARM_CANCEL => self.disarm(),
            WM_APP_STOP => self.stop(),
            WM_APP_REC_DONE => {
                let res = *unsafe { Box::from_raw(lp.0 as *mut Result<RecordingResult>) };
                self.state = State::Idle;
                self.set_recording_look(false);
                match res {
                    Ok(r) => self.open_popup(&r),
                    Err(e) => {
                        crate::log(&format!("recording failed: {e:#}"));
                        message_box(&format!("Recording failed:\n{e:#}"), true);
                    }
                }
            }
            WM_APP_RELOAD => {
                self.settings_open = false;
                self.cfg = Config::load();
                self.register_hotkey();
            }
            _ => {}
        }
        false
    }

    fn toggle(&mut self) {
        match &self.state {
            // Every new recording starts with a fresh drag: the selector posts the
            // region on mouse-up, which arms it (green, live) and waits for Record.
            State::Idle => self.select(),
            State::Selecting(h) => unsafe {
                // The selector posts a cancelled WM_APP_SELECTED when destroyed.
                let _ = DestroyWindow(*h);
            },
            State::Armed(_) => self.start_armed(),
            State::Recording { .. } => self.stop(),
            State::Stopping => {}
        }
    }

    fn select(&mut self) {
        match overlay::select_region(self.hwnd) {
            Ok(h) => self.state = State::Selecting(h),
            Err(e) => message_box(&format!("Couldn't open the region selector: {e}"), true),
        }
    }

    /// Cancels any armed region and lets the user drag a new one.
    fn reselect(&mut self) {
        match self.state {
            State::Idle => self.select(),
            State::Armed(_) => {
                self.disarm();
                self.select();
            }
            _ => {}
        }
    }

    fn arm(&mut self, region: Region) {
        match ArmedUi::show(&region, self.hwnd) {
            Ok(ui) => {
                self.state = State::Armed(ui);
                self.menus.record.set_text(match &self.hotkey {
                    Some(_) => format!("Start recording\t{}", self.cfg.hotkey),
                    None => "Start recording".into(),
                });
            }
            Err(e) => message_box(&format!("Couldn't show the region: {e}"), true),
        }
    }

    fn disarm(&mut self) {
        if let State::Armed(ui) = std::mem::replace(&mut self.state, State::Idle) {
            ui.close();
            self.set_record_label();
        }
    }

    fn start_armed(&mut self) {
        let State::Armed(ui) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let region = ui.region();
        ui.close();
        self.set_record_label();
        if let Some(r) = region {
            self.start_recording(r);
        }
    }

    fn start_recording(&mut self, region: Region) {
        self.cfg = Config::load();
        match capture::start(region, &self.cfg) {
            Ok(rec) => match RecordingUi::show(&region, self.hwnd) {
                Ok(ui) => {
                    self.state = State::Recording { rec, ui };
                    self.set_recording_look(true);
                }
                Err(e) => {
                    let _ = rec.stop();
                    message_box(&format!("Couldn't show the recording UI: {e}"), true);
                }
            },
            Err(e) => {
                crate::log(&format!("capture start failed: {e:#}"));
                message_box(&format!("Couldn't start recording:\n{e:#}"), true);
            }
        }
    }

    fn stop(&mut self) {
        let State::Recording { rec, ui } = std::mem::replace(&mut self.state, State::Stopping) else {
            return;
        };
        ui.close();
        let raw = self.hwnd.0 as isize;
        std::thread::spawn(move || {
            let res = Box::new(rec.stop());
            unsafe {
                let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_REC_DONE, WPARAM(0), LPARAM(Box::into_raw(res) as isize));
            }
        });
    }

    fn set_recording_look(&self, on: bool) {
        let _ = self.tray.set_icon(Some(icon(on)));
        let _ = self.tray.set_tooltip(Some(if on { "screen2imgur — recording" } else { "screen2imgur" }));
        if on {
            self.menus.record.set_text(match &self.hotkey {
                Some(_) => format!("Stop recording\t{}", self.cfg.hotkey),
                None => "Stop recording".into(),
            });
        } else {
            self.set_record_label();
        }
    }

    fn open_settings(&mut self) {
        if self.settings_open {
            return;
        }
        match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("settings").spawn()) {
            Ok(mut child) => {
                self.settings_open = true;
                let raw = self.hwnd.0 as isize;
                std::thread::spawn(move || {
                    let _ = child.wait();
                    unsafe {
                        let _ = PostMessageW(Some(HWND(raw as _)), WM_APP_RELOAD, WPARAM(0), LPARAM(0));
                    }
                });
            }
            Err(e) => message_box(&format!("Couldn't open settings: {e}"), true),
        }
    }

    fn open_popup(&self, r: &RecordingResult) {
        let spawned = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .arg("popup")
                .arg(&r.path)
                .arg(r.width.to_string())
                .arg(r.height.to_string())
                .arg(r.duration_ms.to_string())
                .spawn()
        });
        if let Err(e) = spawned {
            message_box(&format!("Recording saved to {}\nbut the popup failed to open: {e}", r.path.display()), true);
        }
    }
}
