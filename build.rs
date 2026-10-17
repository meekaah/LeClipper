//! Bakes the Imgur API app credentials into the exe.
//! Environment variables win; otherwise they're read from `imgur-app.env`
//! (written by `scripts/setup-imgur-app.ps1`).

const VARS: [&str; 2] = ["SCREEN2IMGUR_CLIENT_ID", "SCREEN2IMGUR_CLIENT_SECRET"];

fn main() {
    println!("cargo:rerun-if-changed=imgur-app.env");
    for var in VARS {
        println!("cargo:rerun-if-env-changed={var}");
    }
    if VARS.iter().all(|v| std::env::var(v).is_ok_and(|s| !s.is_empty())) {
        return;
    }
    let Ok(text) = std::fs::read_to_string("imgur-app.env") else { return };
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            if VARS.contains(&key) && !value.is_empty() && std::env::var(key).map_or(true, |s| s.is_empty()) {
                println!("cargo:rustc-env={key}={value}");
            }
        }
    }
}
