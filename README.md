# LeClipper

Windows tray app that records a region of the screen to MP4 (hardware H.264, up to 60 fps) and saves it as MP4/GIF or uploads it to Imgur. You can also drag the preview straight onto the Imgur upload page, Discord, a folder, …. See [SPEC.md](SPEC.md) for the design decisions.

https://github.com/user-attachments/assets/d851c626-7c8f-4958-9fe2-f49e86532298

## Build

```bash
cargo build --release
```

Output: `target/release/leclipper.exe`, a single ~7 MB exe (Windows 10 2004+ / 11). Uploading needs the Microsoft Edge WebView2 Runtime, which ships with Windows 11 and current Windows 10.

## Use

1. Run `leclipper.exe`. A red dot appears in the tray.
2. Press **Ctrl+Shift+R** (or left-click the tray icon).
3. Drag a rectangle. A click or **Enter** selects the whole monitor; **Esc** or right-click cancels.
4. The region stays on screen with a green border, and you can use your desktop normally. Resize it by dragging its edges or corner handles, and move it by dragging the toolbar's grip. When you're ready, press **● Record** (or the hotkey, or click the tray icon). **✕** cancels.

   Nothing is remembered between recordings: every time you press the hotkey (or the tray's **Record region**) you start over with the full-screen overlay and drag a fresh rectangle. When you release it, the region appears with a green border, live on your desktop, and waits until you press **● Record** (or the hotkey, or the tray icon) again.
5. A red border and a small timer bar show while recording. Neither is captured. Stop with the hotkey, the bar's **Stop** button, or the tray icon.
6. The popup plays the clip on a loop (click it to pause). Choose **Save MP4**, **Save GIF** or **Upload to Imgur**, or drag it somewhere (below).

Right-click the tray icon for Settings, the output folder, and Quit.

## Uploading to Imgur

Imgur no longer hands out API access, so the app uploads through the website itself. **Upload to Imgur** loads imgur.com/upload in a hidden built-in browser window (WebView2, the Edge engine that ships with Windows) and hands the MP4 to the page's own file picker. Imgur sees an ordinary upload from a signed-in browser. The popup shows the upload's progress (with **Cancel**). When it's done, the video's link (`https://i.imgur.com/….gifv`) is copied to the clipboard and shown in the popup with a **Copy** button. Pasted into Discord, it shows as an autoplaying, looping video. (The post page the uploader lands on, `imgur.com/a/…`, only gets a generic "Imgur" card there.) Set **Settings → After recording** to *Upload to Imgur* to make this automatic. Closing the popup mid-upload asks first.

- **Sign-in:** the browser keeps its own profile in `%APPDATA%\LeClipper\webview`, so you stay signed in like in a normal browser. Sign in under **Settings → Imgur → Sign in…**. If you're signed out when uploading, the window pops up so you can sign in, and the upload carries on afterwards. The app never uploads anonymously and never sees your password.
- If Imgur asks for something during an upload (a captcha, say), the window shows itself so you can finish it. Closing the window cancels the upload.
- If an upload fails, the list of requests the Imgur page made is written to `log.txt`, which shows what changed on Imgur's side.
- This relies on how imgur.com/upload is built, so an Imgur redesign can break it. Dragging is the fallback: **Open Imgur** opens the upload page in your normal browser, and you drag the popup's preview onto it. Dragging first saves the clip to your output folder (MP4 or GIF, per the "drag MP4 / drag GIF" selector). The same drag works into Discord, chat apps, Explorer, etc.
- Automated uploads aren't something Imgur's terms bless, so keep it to your own clips at a human pace.

Settings live in `%APPDATA%\LeClipper\config.toml`, and errors are logged to `%APPDATA%\LeClipper\log.txt`.

## How it works

| Piece | File |
|---|---|
| Tray, hotkey, state machine | `src/tray.rs` |
| Region selector, adjustable region, border, stop bar (Win32/GDI) | `src/overlay.rs` |
| Windows.Graphics.Capture → GPU crop → paced frames | `src/capture.rs` |
| Media Foundation sink writer (H.264/MP4) | `src/mp4enc.rs` |
| MP4 decode for GIF/preview | `src/decode.rs` |
| GIF via gifski | `src/gif.rs` |
| Imgur upload through the website in WebView2 | `src/imgur_web.rs` |
| Drag a file out of the popup (shell OLE drag) | `src/shell_drag.rs` |
| Looping preview (streamed, decoded and scaled on the GPU) | `src/preview.rs` |
| Settings / popup windows (egui) | `src/ui_settings.rs`, `src/ui_popup.rs` |

Settings and the popup run as child processes (`leclipper.exe settings`, `leclipper.exe popup …`), so the tray process stays a plain Win32 message loop.

`leclipper.exe imgur upload <file> | signin | signout` runs an Imgur job without the UI (debug builds print its progress).

`leclipper.exe selftest [secs] [w] [h]` records the top-left of the primary monitor and writes `selftest.mp4` + `selftest.gif` to the current directory, for testing the capture/encode path without the UI.

## License note

GIF encoding uses [gifski](https://gif.ski), which is AGPL-3.0. That's fine for personal use; if you distribute the app, it has to comply with the AGPL (or swap in a permissively licensed GIF encoder).
