//! Uploads to Imgur through its own website, since Imgur no longer issues API access.
//! A WebView2 (the Edge engine that ships with Windows) loads imgur.com/upload in a hidden
//! window and gets the file put into the page's file input over the DevTools protocol, so
//! Imgur sees an ordinary upload from a signed-in browser. The profile lives in
//! `%APPDATA%\screen2imgur\webview`, which keeps the Imgur sign-in between runs. When
//! there's no sign-in (or Imgur wants something from the user), the window is shown.
//!
//! Each job runs on its own thread with its own message loop and reports through `Event`s.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler, CallDevToolsProtocolMethodCompletedHandler,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, GetCookiesCompletedHandler,
};
use windows::core::{w, Interface, HSTRING, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const UPLOAD_URL: &str = "https://imgur.com/upload";
const SIGNIN_URL: &str = "https://imgur.com/signin";
const COOKIE_URL: &str = "https://imgur.com";

pub enum Task {
    Upload(PathBuf),
    SignIn,
    SignOut,
}

pub enum Event {
    Status(String),
    /// Upload progress, 0..=1.
    Progress(f32),
    /// The post link for an upload, or a message for sign-in/out.
    Done(String),
    Failed(String),
}

/// A running job. Dropping it doesn't stop it; `cancel` does.
pub struct Job {
    hwnd: Arc<AtomicIsize>,
}

impl Job {
    pub fn cancel(&self) {
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Starts `task` on a new thread. `wake` is called after every event (e.g. to repaint the UI).
pub fn start(task: Task, tx: Sender<Event>, wake: impl Fn() + Send + 'static) -> Job {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let job = Job { hwnd: hwnd.clone() };
    std::thread::spawn(move || {
        let send = |e: Event| {
            let _ = tx.send(e);
            wake();
        };
        let result = run(task, &hwnd, &send);
        hwnd.store(0, Ordering::Relaxed);
        match result {
            Ok(msg) => send(Event::Done(msg)),
            Err(e) => {
                crate::log(&format!("imgur: {e:#}"));
                send(Event::Failed(format!("{e:#}")));
            }
        }
    });
    job
}

fn run(task: Task, hwnd_out: &AtomicIsize, send: &dyn Fn(Event)) -> Result<String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    send(Event::Status("Starting the browser…".into()));
    let b = Browser::new()?;
    hwnd_out.store(b.hwnd.0 as isize, Ordering::Relaxed);
    match task {
        Task::Upload(path) => upload(&b, &path, send),
        Task::SignIn => {
            if b.signed_in()? {
                return Ok("Already signed in to Imgur.".into());
            }
            sign_in(&b, send)?;
            Ok("Signed in to Imgur.".into())
        }
        Task::SignOut => {
            unsafe { b.webview.cast::<ICoreWebView2_2>()?.CookieManager()?.DeleteAllCookies()? };
            Ok("Signed out of Imgur.".into())
        }
    }
}

/// Shows the window on the sign-in page and waits until Imgur sets its session cookie.
fn sign_in(b: &Browser, send: &dyn Fn(Event)) -> Result<()> {
    send(Event::Status("Sign in to Imgur in the window that opened.".into()));
    b.navigate(SIGNIN_URL)?;
    b.show();
    b.wait_until(Duration::from_secs(15 * 60), || Ok(b.signed_in()?.then_some(())))
        .context("Imgur sign-in")?;
    b.hide();
    Ok(())
}

/// Watches the page's own requests to api.imgur.com: progress and errors of the upload,
/// whether the video got attached to the post, and what's still in flight. Signed in, the
/// page creates the post (and moves to `/a/<id>`) *before* uploading, so the URL alone
/// doesn't mean it's done. The request trail is kept for the log.
const UPLOAD_HOOK: &str = r#"(() => {
  const st = window.__s2i = { started: false, loaded: 0, total: 0, uploaded: false, added: false, pending: 0, error: null, log: [] };
  const api = u => /api\.imgur\.com/.test(String(u));
  const done = (m, u, s, b) => {
    st.pending--;
    st.log.push(m + ' ' + String(u).replace(/\?.*/, '') + ' ' + s + ' ' + String(b).slice(0, 200));
    if (m === 'POST' && /\/3\/album\/[^/]+\/add/.test(u) && s < 300) st.added = true;
  };
  const fetch0 = window.fetch;
  window.fetch = async function (input, init) {
    const u = typeof input === 'string' ? input : input.url, m = ((init && init.method) || 'GET').toUpperCase();
    if (!api(u)) return fetch0.apply(this, arguments);
    st.pending++;
    try {
      const r = await fetch0.apply(this, arguments);
      r.clone().text().then(b => done(m, u, r.status, b), () => done(m, u, r.status, ''));
      return r;
    } catch (e) { done(m, u, 0, e); throw e; }
  };
  const open = XMLHttpRequest.prototype.open, send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (m, u) {
    this.__s2i = { m: String(m).toUpperCase(), u, upload: /\/3\/upload(\?|$)/.test(String(u)) };
    return open.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    const x = this.__s2i;
    if (x && api(x.u)) {
      st.pending++;
      this.addEventListener('loadend', () => done(x.m, x.u, this.status, this.responseText));
    }
    if (x && x.upload) {
      st.started = true;
      this.upload.addEventListener('progress', e => { st.loaded = e.loaded; st.total = e.total; });
      this.addEventListener('load', () => {
        if (this.status >= 400) {
          let m = String(this.responseText);
          try { const j = JSON.parse(m); m = (j.data && j.data.error) || m; } catch (e) {}
          st.error = 'Imgur rejected the upload (HTTP ' + this.status + '): ' + String(m).slice(0, 300);
        } else st.uploaded = true;
      });
      this.addEventListener('error', () => { st.error = 'The upload failed (network error).'; });
    }
    return send.apply(this, arguments);
  };
})();"#;

const UPLOAD_STATE: &str = "({ href: location.href, input: !!document.getElementById('file-input'), s: window.__s2i || null })";

fn upload(b: &Browser, path: &std::path::Path, send: &dyn Fn(Event)) -> Result<String> {
    let path = std::path::absolute(path)?;
    b.add_script(UPLOAD_HOOK)?;
    if !b.signed_in()? {
        sign_in(b, send)?;
    }

    send(Event::Status("Opening imgur.com/upload…".into()));
    b.navigate(UPLOAD_URL)?;
    b.wait_until(Duration::from_secs(60), || {
        let st = b.exec(UPLOAD_STATE)?;
        Ok(st["input"].as_bool().unwrap_or(false).then_some(()))
    })
    .context("waiting for Imgur's upload page")?;
    // The session can lapse between the cookie check and the page load; never upload anonymously.
    if !b.signed_in()? {
        sign_in(b, send)?;
        b.navigate(UPLOAD_URL)?;
        b.wait_until(Duration::from_secs(60), || {
            Ok(b.exec(UPLOAD_STATE)?["input"].as_bool().unwrap_or(false).then_some(()))
        })
        .context("waiting for Imgur's upload page")?;
    }

    // Hand the file to the page's own file input, as if picked in its file dialog.
    let node = b.devtools(
        "Runtime.evaluate",
        &serde_json::json!({ "expression": "document.getElementById('file-input')" }),
    )?;
    let object_id = node["result"]["objectId"].as_str().ok_or_else(|| anyhow!("Imgur's file input is gone"))?;
    b.devtools(
        "DOM.setFileInputFiles",
        &serde_json::json!({ "files": [path.to_string_lossy()], "objectId": object_id }),
    )?;
    send(Event::Status("Uploading…".into()));

    let picked = Instant::now();
    let mut shown = false;
    let mut last_progress = -1.0;
    let mut idle_polls = 0;
    let result = b.wait_until(Duration::from_secs(30 * 60), || {
        let st = b.exec(UPLOAD_STATE)?;
        let s = &st["s"];
        if let Some(e) = s["error"].as_str() {
            bail!("{e}");
        }
        let (loaded, total) = (s["loaded"].as_f64().unwrap_or(0.0), s["total"].as_f64().unwrap_or(0.0));
        if total > 0.0 {
            let p = (loaded / total) as f32;
            if p != last_progress {
                last_progress = p;
                send(Event::Progress(p));
                if p >= 1.0 {
                    send(Event::Status("Imgur is processing the video…".into()));
                }
            }
        }
        // A captcha or anything else unexpected: let the user see the page.
        if !s["started"].as_bool().unwrap_or(false) && !shown && picked.elapsed() > Duration::from_secs(20) {
            shown = true;
            b.show();
            send(Event::Status("Imgur needs something. Finish the upload in the window that opened.".into()));
        }
        // Done once the video is uploaded and attached to the post, and the page has
        // gone quiet (it updates the post right after attaching).
        let attached = s["uploaded"].as_bool().unwrap_or(false) && s["added"].as_bool().unwrap_or(false);
        idle_polls = if attached && s["pending"].as_i64() == Some(0) { idle_polls + 1 } else { 0 };
        Ok(post_link(st["href"].as_str().unwrap_or_default()).filter(|_| idle_polls >= 2))
    });
    if result.is_err() {
        if let Some(log) = b.exec("window.__s2i && window.__s2i.log").ok().as_ref().and_then(Value::as_array) {
            for l in log {
                crate::log(&format!("imgur request: {}", l.as_str().unwrap_or_default()));
            }
        }
    }
    result.context("uploading to Imgur")
}

/// After an upload the page moves to the new post, `https://imgur.com/a/<id>`.
fn post_link(href: &str) -> Option<String> {
    let rest = href.strip_prefix("https://imgur.com/a/")?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!id.is_empty()).then(|| format!("https://imgur.com/a/{id}"))
}

thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    /// Set when the user closes the window (or `Job::cancel` posts WM_CLOSE).
    static CLOSED: Cell<bool> = const { Cell::new(false) };
}

struct Browser {
    hwnd: HWND,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}

impl Drop for Browser {
    fn drop(&mut self) {
        unsafe {
            let _ = self.controller.Close();
            CONTROLLER.with(|c| c.borrow_mut().take());
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn wv2err(e: webview2_com::Error) -> anyhow::Error {
    anyhow!("WebView2: {e}")
}

impl Browser {
    fn new() -> Result<Self> {
        static REGISTER: Once = Once::new();
        let hinstance = unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() };
        REGISTER.call_once(|| unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: w!("s2i_imgur"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
        });
        CLOSED.set(false);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("s2i_imgur"),
                w!("Imgur — screen2imgur"),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                560,
                780,
                None,
                None,
                Some(hinstance),
                None,
            )?
        };

        let data_dir = HSTRING::from(crate::config::app_dir().join("webview").as_os_str());
        let (tx, rx) = mpsc::channel();
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(None, &data_dir, None, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, env| {
                err?;
                let _ = tx.send(env);
                Ok(())
            }),
        )
        .map_err(|e| anyhow!("Couldn't start WebView2 ({e}). Is the Microsoft Edge WebView2 Runtime installed?"))?;
        let env = rx.recv()?.ok_or_else(|| anyhow!("WebView2 returned no environment"))?;

        let (tx, rx) = mpsc::channel();
        CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                env.CreateCoreWebView2Controller(hwnd, &handler).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, controller| {
                err?;
                let _ = tx.send(controller);
                Ok(())
            }),
        )
        .map_err(wv2err)?;
        let controller = rx.recv()?.ok_or_else(|| anyhow!("WebView2 returned no controller"))?;

        let webview = unsafe {
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            controller.SetBounds(r)?;
            controller.SetIsVisible(true)?;
            let wv = controller.CoreWebView2()?;
            let s = wv.Settings()?;
            s.SetAreDevToolsEnabled(false)?;
            s.SetIsStatusBarEnabled(false)?;
            wv
        };
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));
        Ok(Self { hwnd, controller, webview })
    }

    fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn navigate(&self, url: &str) -> Result<()> {
        unsafe { self.webview.Navigate(&HSTRING::from(url))? };
        Ok(())
    }

    fn add_script(&self, js: &str) -> Result<()> {
        let wv = self.webview.clone();
        let js = HSTRING::from(js);
        AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                wv.AddScriptToExecuteOnDocumentCreated(&js, &h).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(|err, _id| err),
        )
        .map_err(wv2err)
    }

    /// Runs `js` in the page and returns its result.
    fn exec(&self, js: &str) -> Result<Value> {
        let wv = self.webview.clone();
        let js = HSTRING::from(js);
        let (tx, rx) = mpsc::channel();
        ExecuteScriptCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe { wv.ExecuteScript(&js, &h).map_err(webview2_com::Error::WindowsError) }),
            Box::new(move |err, json| {
                err?;
                let _ = tx.send(json);
                Ok(())
            }),
        )
        .map_err(wv2err)?;
        Ok(serde_json::from_str(&rx.recv()?).unwrap_or(Value::Null))
    }

    fn devtools(&self, method: &str, params: &Value) -> Result<Value> {
        let wv = self.webview.clone();
        let (m, p) = (HSTRING::from(method), HSTRING::from(params.to_string()));
        let (tx, rx) = mpsc::channel();
        CallDevToolsProtocolMethodCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                wv.CallDevToolsProtocolMethod(&m, &p, &h).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, json| {
                err?;
                let _ = tx.send(json);
                Ok(())
            }),
        )
        .map_err(|e| anyhow!("{method}: {e}"))?;
        Ok(serde_json::from_str(&rx.recv()?)?)
    }

    /// Whether the profile holds an Imgur session (Imgur sets `is_authed=1` when signed in).
    fn signed_in(&self) -> Result<bool> {
        let cookies = unsafe { self.webview.cast::<ICoreWebView2_2>()?.CookieManager()? };
        let (tx, rx) = mpsc::channel();
        GetCookiesCompletedHandler::wait_for_async_operation(
            Box::new(move |h| unsafe {
                cookies.GetCookies(&HSTRING::from(COOKIE_URL), &h).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |err, list| {
                err?;
                let mut authed = false;
                if let Some(list) = list {
                    unsafe {
                        let mut n = 0;
                        list.Count(&mut n)?;
                        for i in 0..n {
                            let c = list.GetValueAtIndex(i)?;
                            let (mut name, mut value) = (PWSTR::null(), PWSTR::null());
                            c.Name(&mut name)?;
                            c.Value(&mut value)?;
                            if take_pwstr(name) == "is_authed" && take_pwstr(value) == "1" {
                                authed = true;
                            }
                        }
                    }
                }
                let _ = tx.send(authed);
                Ok(())
            }),
        )
        .map_err(wv2err)?;
        Ok(rx.recv()?)
    }

    /// Polls `check` every 300 ms (pumping messages in between) until it returns a value.
    /// Fails on timeout or when the user closes the window.
    fn wait_until<T>(&self, timeout: Duration, mut check: impl FnMut() -> Result<Option<T>>) -> Result<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if CLOSED.get() {
                bail!("cancelled");
            }
            if let Some(v) = check()? {
                return Ok(v);
            }
            if Instant::now() > deadline {
                bail!("timed out");
            }
            pump_for(Duration::from_millis(300));
        }
    }
}

fn pump_for(d: Duration) {
    let deadline = Instant::now() + d;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        unsafe {
            MsgWaitForMultipleObjects(None, false, left.as_millis() as u32, QS_ALLINPUT);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_SIZE => {
                CONTROLLER.with(|c| {
                    if let Some(c) = &*c.borrow() {
                        let mut r = RECT::default();
                        let _ = GetClientRect(hwnd, &mut r);
                        let _ = c.SetBounds(r);
                    }
                });
                LRESULT(0)
            }
            // Closing cancels the job; the job thread destroys the window.
            WM_CLOSE => {
                CLOSED.set(true);
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
