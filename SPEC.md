# screen2imgur — v1 spec

Windows tray app (Rust) that records a selected screen region to MP4/GIF and saves it to disk or uploads it to Imgur.

## Decisions

| Area | Decision |
|---|---|
| Platform | Windows 10 1903+ / 11, x86_64, single `.exe` |
| Language | Rust (stable, MSVC toolchain) |
| Trigger | Tray icon + global hotkey (default `Ctrl+Shift+R`: start region select; press again to stop) |
| Region | Drag a rectangle on a dimmed full-screen overlay; one monitor at a time; per-monitor DPI aware; `Esc` cancels |
| Capture | Windows.Graphics.Capture (GPU), cropped to the region |
| Cursor | Captured by default, toggle in settings |
| Audio | None |
| Framerate | 15 / 30 / 60 fps presets (default 60 for MP4, 30 for GIF) |
| Video encode | Media Foundation hardware H.264 → MP4, quality/bitrate slider, no external binaries |
| GIF encode | `gifski` (high-quality palettes), configurable fps + max width |
| While recording | Thin red border outside the region + small floating bar (timer, Stop); both excluded from capture |
| After stop | Quick-action popup: preview + **Save MP4**, **Save GIF**, **Upload to Imgur**, **Copy link**; configurable default action |
| Imgur | OAuth2 login via browser + loopback redirect; uploads go to the user's account, optional target album. Imgur app credentials are compiled in (`SCREEN2IMGUR_CLIENT_ID/SECRET`), with an optional per-user override |
| Secrets | Client ID/Secret + access/refresh tokens in Windows Credential Manager |
| Settings | `%APPDATA%\screen2imgur\config.toml`, edited in an egui settings window |
| Output dir | Default `%USERPROFILE%\Videos\screen2imgur`, filename `screen2imgur_YYYY-MM-DD_HH-MM-SS.{mp4,gif}` |

## Planned crates

- `windows` / `windows-capture`: WGC capture, D3D11, Media Foundation encoder
- `tray-icon`, `global-hotkey`, `winit`: tray, hotkey, event loop
- `eframe`/`egui`: settings window and post-record popup
- `gifski` + `imgref`/`rgb`: GIF encoding
- `reqwest` (rustls) + `tiny_http`: Imgur API and OAuth loopback
- `keyring`: Credential Manager
- `serde` + `toml`, `directories`, `arboard` (clipboard), `notify-rust` (toasts)

## Imgur notes / risks

- The builder registers one app (once) at https://api.imgur.com/oauth2/addclient with type "OAuth 2 authorization with a callback URL" and callback `http://localhost:47892/callback`. Imgur has closed new registrations at times, so this is the first thing to check.
- Imgur caps uploads (videos roughly 60 s / 200 MB; big GIFs get converted to MP4/GIFV). The app warns before uploading anything over the limit.
- Free API usage is rate-limited per client and per user. Fine for personal use.

## Out of scope for v1

Audio, trimming/editing, regions spanning monitors, click highlights, WebM/AV1, macOS/Linux.
