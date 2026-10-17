# screen2imgur

Windows tray app that records a region of the screen to MP4 (hardware H.264, up to 60 fps) and saves it as MP4/GIF. To share it, drag the preview straight onto the Imgur upload page (or Discord, a folder, …). See [SPEC.md](SPEC.md) for the design decisions.

## Build

```bash
cargo build --release
```

Output: `target/release/screen2imgur.exe`, a single ~10 MB exe with no runtime dependencies (Windows 10 2004+ / 11).

## Use

1. Run `screen2imgur.exe`. A red dot appears in the tray.
2. Press **Ctrl+Shift+R** (or left-click the tray icon).
3. Drag a rectangle. A click or **Enter** selects the whole monitor; **Esc** or right-click cancels.
4. The region stays on screen with a green border, and you can use your desktop normally. Resize it by dragging its edges or corner handles, and move it by dragging the toolbar's grip. When you're ready, press **● Record** (or the hotkey, or click the tray icon). **✕** cancels.

   The next time you press the hotkey (or the tray's **Record region**), the last region comes back right away, ready to adjust or record. It's kept in memory until you quit, and dropped if a monitor change leaves it no longer on screen. To drag a fresh one, use the tray's **Select new region**.
5. A red border and a small timer bar show while recording. Neither is captured. Stop with the hotkey, the bar's **Stop** button, or the tray icon.
6. The popup plays the clip on a loop (click it to pause). Choose **Save MP4** or **Save GIF**, or share it (below).

Right-click the tray icon for Settings, the output folder, and Quit.

## Sharing to Imgur (drag and drop)

Imgur no longer hands out API access, so there's no direct upload. Instead:

1. Click **Open Imgur upload** in the popup. It opens <https://imgur.com/upload> in your browser.
2. Drag the popup's preview onto that page and drop it. Imgur uploads it like any file dropped from Explorer.

Dragging first saves the clip to your output folder (MP4 or GIF, picked by the "drag MP4 / drag GIF" selector next to the button). The browser reads that saved file, so closing the popup mid-upload is safe. The same drag works into Discord, chat apps, Explorer, etc. If you choose GIF and it hasn't been made yet, the first drag starts the export; drag again once it's saved.

Settings live in `%APPDATA%\screen2imgur\config.toml`, and errors are logged to `%APPDATA%\screen2imgur\log.txt`.

## How it works

| Piece | File |
|---|---|
| Tray, hotkey, state machine | `src/tray.rs` |
| Region selector, adjustable region, border, stop bar (Win32/GDI) | `src/overlay.rs` |
| Windows.Graphics.Capture → GPU crop → paced frames | `src/capture.rs` |
| Media Foundation sink writer (H.264/MP4) | `src/mp4enc.rs` |
| MP4 decode for GIF/preview | `src/decode.rs` |
| GIF via gifski | `src/gif.rs` |
| Drag a file out of the popup (shell OLE drag) | `src/shell_drag.rs` |
| Looping preview (streamed, decoded and scaled on the GPU) | `src/preview.rs` |
| Settings / popup windows (egui) | `src/ui_settings.rs`, `src/ui_popup.rs` |

Settings and the popup run as child processes (`screen2imgur.exe settings`, `screen2imgur.exe popup …`), so the tray process stays a plain Win32 message loop.

`screen2imgur.exe selftest [secs] [w] [h]` records the top-left of the primary monitor and writes `selftest.mp4` + `selftest.gif` to the current directory, for testing the capture/encode path without the UI.

## License note

GIF encoding uses [gifski](https://gif.ski), which is AGPL-3.0. That's fine for personal use; if you distribute the app, it has to comply with the AGPL (or swap in a permissively licensed GIF encoder).
