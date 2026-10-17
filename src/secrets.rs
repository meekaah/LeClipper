//! Imgur credentials, stored in Windows Credential Manager.

use anyhow::Result;
use serde::{Deserialize, Serialize};

const SERVICE: &str = "screen2imgur";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub account_username: String,
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

pub fn client_id() -> Option<String> {
    get("client_id")
}

pub fn client_secret() -> Option<String> {
    get("client_secret")
}

pub fn set_client(id: &str, secret: &str) -> Result<()> {
    set("client_id", id.trim())?;
    set("client_secret", secret.trim())
}

pub fn tokens() -> Option<Tokens> {
    serde_json::from_str(&get("oauth")?).ok()
}

pub fn set_tokens(t: Option<&Tokens>) -> Result<()> {
    match t {
        Some(t) => set("oauth", &serde_json::to_string(t)?),
        None => set("oauth", ""),
    }
}
