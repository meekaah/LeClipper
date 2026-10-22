# screen2imgur — v1 spec

Windows tray app (Rust) that records a selected screen region to MP4/GIF and saves it to disk or uploads it to Imgur through the website.

## Decisions

| Area | Decision |
|---|---|
| Platform | Windows 10 1903+ / 11, x86_64, single `.exe` |
| Language | Rust (stable, MSVC toolchain) |
| Trigger | Tray icon + global hotkey (default `Ctrl+Shift+R`: start region select; press again to start recording, then again to stop) |
| Region | Drag a rectangle on a dimmed full-screen overlay; one monitor at a time; per-monitor DPI aware; `Esc` cancels |
| Before recording | Green border around the live (unfrozen) region, resizable by its edges/handles, movable via a toolbar with **Record** and cancel; both excluded from capture |
| Repeat | Nothing is remembered between recordings: every trigger drags a fresh rectangle on the full-screen overlay, and on release the green border is drawn around that live (unfrozen) region, waiting for **Record**. Tray **Select new region** cancels any armed region and drags a new one |
| Capture | Windows.Graphics.Capture (GPU), cropped to the region |
| Cursor | Captured by default, toggle in settings |
| Audio | None |
| Framerate | 15 / 30 / 60 fps presets (default 60 for MP4, 30 for GIF) |
| Video encode | Media Foundation hardware H.264 → MP4, quality/bitrate slider, no external binaries |
| GIF encode | `gifski` (high-quality palettes), configurable fps + max width |
| While recording | Thin red border outside the region + small floating bar (timer, Stop); both excluded from capture |
| After stop | Quick-action popup: looping preview (click to pause, drag to share) + **Save MP4**, **Save GIF**, **Upload to Imgur**, **Open Imgur**; configurable default action (incl. upload) |
| Imgur upload | No API (Imgur stopped issuing API access). Uploads drive imgur.com/upload in a hidden WebView2 with a persistent profile; always the MP4. Result: the video's `i.imgur.com/<id>.gifv` link (Discord embeds it as an autoplaying looping video), copied to the clipboard and shown in the popup with progress and Cancel. See *Imgur notes* |
| Imgur sign-in | Required; uploads are never anonymous. Settings → Imgur has **Sign in…** / **Sign out**; when signed out at upload time the window shows imgur.com/signin and the upload continues after |
| Imgur fallback | **Open Imgur** opens imgur.com/upload in the default browser; the preview is a drag source (shell `SHDoDragDrop`) that drops the saved MP4/GIF there, or into Discord, Explorer… |
| Settings | `%APPDATA%\screen2imgur\config.toml`, edited in an egui settings window |
| Output dir | Default `%USERPROFILE%\Videos\screen2imgur`, filename `screen2imgur_YYYY-MM-DD_HH-MM-SS.{mp4,gif}` |

## Planned crates

- `windows` / `windows-capture`: WGC capture, D3D11, Media Foundation encoder
- `tray-icon`, `global-hotkey`, `winit`: tray, hotkey, event loop
- `eframe`/`egui`: settings window and post-record popup
- `gifski` + `imgref`/`rgb`: GIF encoding
- `serde` + `toml`, `directories`, `rfd` (folder picker), `open`
- `webview2-com`: WebView2 for the Imgur upload (loader linked statically, so still a single exe; the runtime ships with Windows 11 and current Windows 10)

## Imgur notes

- Imgur no longer grants API access, so the old OAuth code is gone. Instead `src/imgur_web.rs` runs imgur.com in a WebView2 and lets the site's own uploader do the work; we only hand it the file.
- **Profile:** `%APPDATA%\screen2imgur\webview`, so cookies (the Imgur session) persist like in a normal browser. Signed in = cookie `is_authed=1` on imgur.com. Sign-out deletes the profile's cookies.
- **Window:** hidden while uploading. It shows for sign-in, and if the upload request hasn't started 20 s after the file was handed over (captcha or anything unexpected), so the user can finish in the page. Closing it cancels the job.
- **Feeding the file:** CDP `Runtime.evaluate` for `#file-input`, then `DOM.setFileInputFiles` with the MP4 path: the same as picking the file in the page's dialog.
- **What the page does:** create a hidden album (`POST /3/album`), `POST /3/upload/checkcaptcha`, upload the file (`POST /3/upload`, XHR), poll `/media/v1/media/<id>/status` until processed, attach it (`POST /3/album/<deletehash>/add`), then update the album (`PUT /3/album/<id>`). All against `api.imgur.com` with the site's own client ID and session cookie.
- **When it's done:** signed in, the page moves to `imgur.com/a/<id>` right after creating the album, *before* uploading, so the URL means nothing. A script injected at document creation wraps `fetch`/XHR to `api.imgur.com` and tracks upload progress and errors, the video id from the `/3/upload` response, whether `/add` succeeded, and how many requests are in flight. The job finishes when the upload and `/add` succeeded and nothing is in flight for two polls in a row (300 ms apart).
- **Which link:** `https://i.imgur.com/<id>.gifv`, not the album. The album the uploader makes is untitled and not on the account (`privacy: private`), and Imgur gives crawlers only a generic "Imgur" card for it (no `og:video`), which is what Discord showed. The `.gifv` page carries `og:video` → `i.imgur.com/<id>.mp4`, which Discord plays inline, looping and muted.
- **Diagnostics:** if an upload fails or times out (30 min), the request trail goes to `log.txt`. `screen2imgur.exe imgur upload <file> | signin | signout` runs a job without the UI.
- **Fragility:** this depends on the upload page (`#file-input`, the request sequence above). A redesign can break it; drag and drop is the fallback.
- Imgur still caps uploads (videos roughly 60 s / 200 MB).

## Out of scope for v1

Audio, trimming/editing, regions spanning monitors, click highlights, WebM/AV1, macOS/Linux.
