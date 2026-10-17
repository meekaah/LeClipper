# screen2imgur

Windows tray app that records a region of the screen to MP4 (hardware H.264, up to 60 fps) and saves it as MP4/GIF or uploads it to your Imgur account. See [SPEC.md](SPEC.md) for the design decisions.

## Build

```bash
cargo build --release
```

Output: `target/release/screen2imgur.exe`, a single ~10 MB exe with no runtime dependencies (Windows 10 2004+ / 11).

## Use

1. Run `screen2imgur.exe`. A red dot appears in the tray.
2. Press **Ctrl+Shift+R** (or left-click the tray icon).
3. Drag a rectangle. A click or **Enter** selects the whole monitor; **Esc** or right-click cancels.
4. A red border and a small timer bar show while recording. Neither is captured. Stop with the hotkey, the bar's **Stop** button, or the tray icon.
5. In the popup, choose **Save MP4**, **Save GIF**, or **Upload to Imgur** (as MP4 or GIF), then **Copy link**.

Right-click the tray icon for Settings, the output folder, and Quit.

## Imgur setup

1. Register an app at <https://api.imgur.com/oauth2/addclient>:
   - Authorization type: **OAuth 2 authorization with a callback URL**
   - Callback URL: `http://localhost:47892/callback`
2. In Settings, enter the Client ID and Client Secret, then click **Log in with Imgur** and approve in the browser.
3. Optionally click **Load albums** and pick a target album.

The Client ID/Secret and OAuth tokens are stored in Windows Credential Manager (entries named `screen2imgur`). Other settings live in `%APPDATA%\screen2imgur\config.toml`, and errors are logged to `%APPDATA%\screen2imgur\log.txt`.

Imgur limits videos to about 60 s / 200 MB.

## How it works

| Piece | File |
|---|---|
| Tray, hotkey, state machine | `src/tray.rs` |
| Region selector, border, stop bar (Win32/GDI) | `src/overlay.rs` |
| Windows.Graphics.Capture → GPU crop → paced frames | `src/capture.rs` |
| Media Foundation sink writer (H.264/MP4) | `src/mp4enc.rs` |
| MP4 decode for GIF/preview | `src/decode.rs` |
| GIF via gifski | `src/gif.rs` |
| OAuth, upload, albums | `src/imgur.rs` |
| Settings / popup windows (egui) | `src/ui_settings.rs`, `src/ui_popup.rs` |

Settings and the popup run as child processes (`screen2imgur.exe settings`, `screen2imgur.exe popup …`), so the tray process stays a plain Win32 message loop.

`screen2imgur.exe selftest [secs] [w] [h]` records the top-left of the primary monitor and writes `selftest.mp4` + `selftest.gif` to the current directory, for testing the capture/encode path without the UI.

## License note

GIF encoding uses [gifski](https://gif.ski), which is AGPL-3.0. That's fine for personal use; if you distribute the app, it has to comply with the AGPL (or swap in a permissively licensed GIF encoder).
