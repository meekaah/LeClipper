//! Imgur API: OAuth login via a loopback redirect, token refresh, uploads, album listing.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::secrets::{self, Tokens};

pub const CALLBACK_PORT: u16 = 47892;
pub const CALLBACK_URL: &str = "http://localhost:47892/callback";
const API: &str = "https://api.imgur.com";

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("screen2imgur/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30 * 60))
        .build()?)
}

fn credentials() -> Result<(String, String)> {
    secrets::client().context(
        "This build has no built-in Imgur app. Add your own under Settings → Imgur → Advanced.",
    )
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    account_username: String,
}

impl TokenResponse {
    fn into_tokens(self, client_id: &str) -> Tokens {
        Tokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: now() + self.expires_in,
            account_username: self.account_username,
            client_id: client_id.to_string(),
        }
    }
}

fn token_request(form: &[(&str, &str)]) -> Result<Tokens> {
    let resp = client()?.post(format!("{API}/oauth2/token")).form(form).send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        bail!("Imgur token request failed ({status}): {}", api_error(&body));
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected token response")?;
    let client_id = form.iter().find(|(k, _)| *k == "client_id").map(|(_, v)| *v).unwrap_or_default();
    Ok(t.into_tokens(client_id))
}

fn refresh(tokens: &Tokens) -> Result<Tokens> {
    let (id, secret) = credentials()?;
    let mut t = token_request(&[
        ("refresh_token", tokens.refresh_token.as_str()),
        ("client_id", id.as_str()),
        ("client_secret", secret.as_str()),
        ("grant_type", "refresh_token"),
    ])?;
    if t.account_username.is_empty() {
        t.account_username = tokens.account_username.clone();
    }
    secrets::set_tokens(Some(&t))?;
    Ok(t)
}

/// A valid access token, refreshing it if it's about to expire.
fn access_token(force_refresh: bool) -> Result<String> {
    let t = secrets::tokens().context("Not logged in to Imgur. Open Settings and click Log in.")?;
    if force_refresh || t.expires_at - 120 < now() {
        return Ok(refresh(&t)?.access_token);
    }
    Ok(t.access_token)
}

/// Opens the browser for consent and waits (up to 5 minutes) for the redirect.
/// `cancel` aborts the wait.
pub fn login(cancel: Arc<AtomicBool>) -> Result<Tokens> {
    let (client_id, client_secret) = credentials()?;
    let state = format!("{:x}{:x}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));

    // Browsers may resolve "localhost" to either address family, so listen on both.
    let servers: Vec<tiny_http::Server> = [
        format!("127.0.0.1:{CALLBACK_PORT}"),
        format!("[::1]:{CALLBACK_PORT}"),
    ]
    .iter()
    .filter_map(|addr| tiny_http::Server::http(addr).ok())
    .collect();
    if servers.is_empty() {
        bail!("port {CALLBACK_PORT} is in use; close whatever is using it and try again");
    }

    let url = format!(
        "{API}/oauth2/authorize?client_id={}&response_type=token&state={}",
        urlencode(&client_id),
        state
    );
    open::that(&url).context("could not open the browser")?;

    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            bail!("login cancelled");
        }
        for server in &servers {
            let Ok(Some(req)) = server.recv_timeout(Duration::from_millis(100)) else { continue };
            let raw_url = req.url().to_string();
            let (path, query) = raw_url.split_once('?').unwrap_or((&raw_url, ""));
            let params = parse_query(query);
            let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());

            match path {
                "/callback" => {
                    if let Some(err) = get("error") {
                        respond(req, 400, &page("Imgur login failed", err));
                        bail!("Imgur denied the login: {err}");
                    }
                    if let Some(code) = get("code") {
                        // Authorization-code flow (in case the app is configured that way).
                        if get("state") != Some(state.as_str()) {
                            respond(req, 400, &page("Login failed", "state mismatch"));
                            bail!("OAuth state mismatch");
                        }
                        let t = token_request(&[
                            ("client_id", client_id.as_str()),
                            ("client_secret", client_secret.as_str()),
                            ("grant_type", "authorization_code"),
                            ("code", code),
                        ]);
                        return finish(req, t);
                    }
                    // Token flow: the token is in the URL fragment, which only the page can see.
                    respond(req, 200, RELAY_PAGE);
                }
                "/token" => {
                    if get("state") != Some(state.as_str()) {
                        respond(req, 400, "state mismatch");
                        bail!("OAuth state mismatch");
                    }
                    let t = (|| -> Result<Tokens> {
                        Ok(Tokens {
                            access_token: get("access_token").context("missing access_token")?.into(),
                            refresh_token: get("refresh_token").context("missing refresh_token")?.into(),
                            expires_at: now() + get("expires_in").and_then(|v| v.parse().ok()).unwrap_or(3600),
                            account_username: get("account_username").unwrap_or_default().into(),
                            client_id: client_id.clone(),
                        })
                    })();
                    return finish(req, t);
                }
                _ => respond(req, 404, "not found"),
            }
        }
    }
    bail!("timed out waiting for the Imgur login")
}

fn finish(req: tiny_http::Request, t: Result<Tokens>) -> Result<Tokens> {
    match t {
        Ok(t) => {
            secrets::set_tokens(Some(&t))?;
            respond(req, 200, &page("Logged in", "You can close this tab and go back to screen2imgur."));
            Ok(t)
        }
        Err(e) => {
            respond(req, 500, &page("Login failed", &format!("{e:#}")));
            Err(e)
        }
    }
}

const RELAY_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>screen2imgur</title>
<body style="font-family:system-ui;padding:2em"><p id="m">Finishing login…</p>
<script>
const h = location.hash.slice(1);
if (!h) { document.getElementById('m').textContent = 'No token received. Close this tab and try again.'; }
else fetch('/token?' + h).then(r => r.text()).then(t => { document.open(); document.write(t); document.close(); history.replaceState(null, '', '/callback'); });
</script>"#;

fn page(title: &str, msg: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>screen2imgur</title><body style=\"font-family:system-ui;padding:2em\"><h2>{}</h2><p>{}</p>",
        esc(title),
        esc(msg)
    )
}

fn respond(req: tiny_http::Request, code: u16, body: &str) {
    let header = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
    let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code).with_header(header));
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (urldecode(k), urldecode(v))
        })
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|c| match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (c as char).to_string(),
            _ => format!("%{c:02X}"),
        })
        .collect()
}

pub fn logout() -> Result<()> {
    secrets::set_tokens(None)
}

/// Pulls a readable message out of an Imgur error body.
fn api_error(body: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return body.chars().take(300).collect(),
    };
    let e = &v["data"]["error"];
    if let Some(s) = e.as_str() {
        return s.to_string();
    }
    if let Some(s) = e["message"].as_str() {
        return s.to_string();
    }
    if let Some(s) = v["error_description"].as_str().or(v["error"].as_str()) {
        return s.to_string();
    }
    body.chars().take(300).collect()
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: String,
    pub title: String,
}

pub fn albums() -> Result<Vec<Album>> {
    let mut retried = false;
    loop {
        let token = access_token(retried)?;
        let resp = client()?
            .get(format!("{API}/3/account/me/albums"))
            .bearer_auth(token)
            .send()?;
        let status = resp.status();
        let body = resp.text()?;
        if status.as_u16() == 401 && !retried {
            retried = true;
            continue;
        }
        if !status.is_success() {
            bail!("listing albums failed ({status}): {}", api_error(&body));
        }
        let v: serde_json::Value = serde_json::from_str(&body)?;
        let list = v["data"].as_array().cloned().unwrap_or_default();
        return Ok(list
            .iter()
            .filter_map(|a| {
                Some(Album {
                    id: a["id"].as_str()?.to_string(),
                    title: a["title"].as_str().filter(|t| !t.is_empty()).unwrap_or("(untitled)").to_string(),
                })
            })
            .collect());
    }
}

#[derive(Debug, Clone)]
pub struct Uploaded {
    pub id: String,
    pub direct_link: String,
    pub page_link: String,
    pub deletehash: String,
}

/// Counts bytes as reqwest reads the file.
struct CountingReader<R> {
    inner: R,
    sent: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

pub fn upload(path: &Path, title: &str, album: Option<&str>, sent: Arc<AtomicU64>) -> Result<Uploaded> {
    let is_video = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    let (field, mime) = if is_video { ("video", "video/mp4") } else { ("image", "image/gif") };
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("capture").to_string();
    let len = std::fs::metadata(path)?.len();

    let mut retried = false;
    loop {
        sent.store(0, Ordering::Relaxed);
        let token = access_token(retried)?;
        let file = std::fs::File::open(path)?;
        let part = reqwest::blocking::multipart::Part::reader_with_length(
            CountingReader { inner: file, sent: sent.clone() },
            len,
        )
        .file_name(file_name.clone())
        .mime_str(mime)?;
        let mut form = reqwest::blocking::multipart::Form::new()
            .part(field, part)
            .text("type", "file")
            .text("title", title.to_string());
        if is_video {
            form = form.text("disable_audio", "1");
        }
        if let Some(a) = album.filter(|a| !a.is_empty()) {
            form = form.text("album", a.to_string());
        }

        let resp = client()?
            .post(format!("{API}/3/upload"))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .context("upload request failed")?;
        let status = resp.status();
        let body = resp.text()?;
        if status.as_u16() == 401 && !retried {
            retried = true;
            continue;
        }
        if !status.is_success() {
            bail!("Imgur upload failed ({status}): {}", api_error(&body));
        }
        let v: serde_json::Value = serde_json::from_str(&body).context("unexpected upload response")?;
        let d = &v["data"];
        let id = d["id"].as_str().ok_or_else(|| anyhow!("upload response has no id: {body}"))?.to_string();
        let direct_link = d["link"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://i.imgur.com/{id}.{}", if is_video { "mp4" } else { "gif" }));
        return Ok(Uploaded {
            page_link: format!("https://imgur.com/{id}"),
            direct_link,
            deletehash: d["deletehash"].as_str().unwrap_or_default().to_string(),
            id,
        });
    }
}
