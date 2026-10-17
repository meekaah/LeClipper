//! Imgur credentials, stored in Windows Credential Manager.
//!
//! The Imgur API app (Client ID/Secret) is registered once by whoever builds the exe and
//! baked in at compile time via `SCREEN2IMGUR_CLIENT_ID` / `SCREEN2IMGUR_CLIENT_SECRET`.
//! Users can override it with their own app in Settings → Imgur → Advanced.

use anyhow::Result;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "screen2imgur";

const BUILTIN_ID: Option<&str> = option_env!("SCREEN2IMGUR_CLIENT_ID");
const BUILTIN_SECRET: Option<&str> = option_env!("SCREEN2IMGUR_CLIENT_SECRET");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub account_username: String,
    /// The Imgur app that issued these tokens; they're useless with any other.
    #[serde(default)]
    pub client_id: String,
}

fn entry(name: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(SERVICE, name)?)
}

fn get(name: &str) -> Option<String> {
    entry(name).ok()?.get_password().ok().filter(|s| !s.is_empty())
}

fn set(name: &str, value: &str) -> Result<()> {
    let e = entry(name)?;
    if value.is_empty() {
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.into()),
        }
    } else {
        Ok(e.set_password(value)?)
    }
}

/// The Client ID/Secret compiled into this build, if any.
pub fn builtin_client() -> Option<(String, String)> {
    let id = BUILTIN_ID.map(str::trim).filter(|s| !s.is_empty())?;
    let secret = BUILTIN_SECRET.map(str::trim).filter(|s| !s.is_empty())?;
    Some((id.to_string(), secret.to_string()))
}

/// The user's own Imgur app from Settings → Advanced (both fields may be empty).
pub fn custom_client() -> (String, String) {
    (get("client_id").unwrap_or_default(), get("client_secret").unwrap_or_default())
}

pub fn set_custom_client(id: &str, secret: &str) -> Result<()> {
    set("client_id", id.trim())?;
    set("client_secret", secret.trim())
}

/// The app to talk to Imgur with: the user's own if fully set, otherwise the built-in one.
pub fn client() -> Option<(String, String)> {
    match custom_client() {
        (id, secret) if !id.is_empty() && !secret.is_empty() => Some((id, secret)),
        _ => builtin_client(),
    }
}

/// Saved tokens, if they belong to the app currently in use.
pub fn tokens() -> Option<Tokens> {
    let t: Tokens = serde_json::from_str(&get("oauth")?).ok()?;
    let current = client()?.0;
    (t.client_id.is_empty() || t.client_id == current).then_some(t)
}

pub fn set_tokens(t: Option<&Tokens>) -> Result<()> {
    match t {
        Some(t) => set("oauth", &serde_json::to_string(t)?),
        None => set("oauth", ""),
    }
}
