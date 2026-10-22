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
| Repeat | The last region (as adjusted) is kept in memory; the next trigger goes straight to the green border there, if it still lies entirely on one monitor. Tray **Select new region** drags a new one |
| Capture | Windows.Graphics.Capture (GPU), cropped to the region |
| Cursor | Captured by default, toggle in settings |
| Audio | None |
| Framerate | 15 / 30 / 60 fps presets (default 60 for MP4, 30 for GIF) |
| Video encode | Media Foundation hardware H.264 → MP4, quality/bitrate slider, no external binaries |
| GIF encode | `gifski` (high-quality palettes), configurable fps + max width |
| While recording | Thin red border outside the region + small floating bar (timer, Stop); both excluded from capture |
| After stop | Quick-action popup: looping preview (click to pause, drag to share) + **Save MP4**, **Save GIF**, **Upload to Imgur**, **Open Imgur**; configurable default action (incl. upload) |
| Imgur | No API (Imgur stopped issuing API access). Uploads drive imgur.com/upload in a hidden WebView2 with a persistent profile: the MP4 goes into the page's `#file-input` via CDP `DOM.setFileInputFiles`, and the link is the `imgur.com/a/<id>` page it lands on. Sign-in is required (cookie `is_authed=1`); when signed out the window shows for the user to sign in. Fallback: the preview is a drag source (shell `SHDoDragDrop`) for dropping onto the page in a normal browser |
| Settings | `%APPDATA%\screen2imgur\config.toml`, edited in an egui settings window |
| Output dir | Default `%USERPROFILE%\Videos\screen2imgur`, filename `screen2imgur_YYYY-MM-DD_HH-MM-SS.{mp4,gif}` |

## Planned crates

- `windows` / `windows-capture`: WGC capture, D3D11, Media Foundation encoder
- `tray-icon`, `global-hotkey`, `winit`: tray, hotkey, event loop
- `eframe`/`egui`: settings window and post-record popup
- `gifski` + `imgref`/`rgb`: GIF encoding
- `serde` + `toml`, `directories`, `rfd` (folder picker), `open`

## Imgur notes

- Imgur no longer grants API access, so the old OAuth code is gone. The website's uploader creates a hidden album, uploads to `api.imgur.com/3/upload` with the site's own client ID, then moves the page to `imgur.com/a/<id>`. We let the page do all of that and only feed it the file, hook its upload XHR for progress/errors, and wait for that URL.
- Imgur still caps uploads (videos roughly 60 s / 200 MB).

## Out of scope for v1

Audio, trimming/editing, regions spanning monitors, click highlights, WebM/AV1, macOS/Linux.
