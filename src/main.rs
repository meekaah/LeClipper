#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod config;
mod decode;
mod gif;
mod hdr;
mod icon;
mod imgur_web;
mod mp4enc;
mod overlay;
mod preview;
mod shell_drag;
mod tray;
mod ui_popup;
mod ui_settings;

use std::io::Write;
use std::path::PathBuf;

/// Appends a line to `%APPDATA%\screen2imgur\log.txt` (and stderr in debug builds).
pub fn log(msg: &str) {
    let line = format!("{} [{}] {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), std::process::id());
    #[cfg(debug_assertions)]
    eprint!("{line}");
    let _ = std::fs::create_dir_all(config::app_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::app_dir().join("log.txt")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `screen2imgur selftest [secs] [w] [h]`: records the top-left of the primary monitor
/// with the current settings, then converts it to GIF. Writes both next to the exe's cwd.
fn selftest(args: &[String]) -> anyhow::Result<()> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    let num = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (secs, w, h) = (num(0, 3), num(1, 640) as u32 & !1, num(2, 360) as u32 & !1);
    let cfg = config::Config::load();
    let mon = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
    };
    let region = capture::Region { monitor: mon.0 as isize, x: 0, y: 0, w, h, mon_x: 0, mon_y: 0 };
    let t = std::time::Instant::now();
    let rec = capture::start(region, &cfg)?;
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let r = rec.stop()?;
    let mp4 = std::path::PathBuf::from("selftest.mp4");
    std::fs::copy(&r.path, &mp4)?;
    let _ = std::fs::remove_file(&r.path);
    println!(
        "mp4: {}x{} {} ms, {} bytes (took {:?})",
        r.width, r.height, r.duration_ms, std::fs::metadata(&mp4)?.len(), t.elapsed()
    );

    let mut frames = 0;
    decode::for_each_frame(&mp4, Some((w, h)), |_| {
        frames += 1;
        Ok(true)
    })?;
    println!("decoded {frames} frames");

    let t = std::time::Instant::now();
    let opts = gif::GifOptions { fps: cfg.gif_fps, max_width: cfg.gif_max_width, quality: cfg.gif_quality };
    let prog = std::sync::Arc::new(gif::Progress::default());
    gif::mp4_to_gif(&mp4, std::path::Path::new("selftest.gif"), (w, h), &opts, prog.clone())?;
    println!(
        "gif: {} frames in, {} out, {} bytes (took {:?})",
        prog.frames_in.load(std::sync::atomic::Ordering::Relaxed),
        prog.frames_out.load(std::sync::atomic::Ordering::Relaxed),
        std::fs::metadata("selftest.gif")?.len(),
        t.elapsed()
    );
    Ok(())
}

/// `screen2imgur imgur upload <file> | signin | signout`: runs an Imgur job without the UI
/// and prints its events (visible in debug builds, which have a console).
fn imgur_cli(args: &[String]) -> anyhow::Result<()> {
    use imgur_web::{Event, Task};
    let task = match (args.first().map(String::as_str), args.get(1)) {
        (Some("upload"), Some(f)) => Task::Upload(PathBuf::from(f)),
        (Some("signin"), _) => Task::SignIn,
        (Some("signout"), _) => Task::SignOut,
        _ => anyhow::bail!("usage: screen2imgur imgur upload <file> | signin | signout"),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _job = imgur_web::start(task, tx, || {});
    for ev in rx {
        match ev {
            Event::Status(s) => println!("{s}"),
            Event::Progress(p) => println!("{:.0}%", p * 100.0),
            Event::Account(_) => {}
            Event::Done(s) => {
                println!("done: {s}");
                return Ok(());
            }
            Event::Failed(e) => anyhow::bail!(e),
        }
    }
    Ok(())
}

fn main() {
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("settings") => ui_settings::run(),
        Some("popup") => {
            let parse = |i: usize| args.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            match args.get(1) {
                Some(path) => ui_popup::run(ui_popup::Args {
                    path: PathBuf::from(path),
                    width: parse(2) as u32,
                    height: parse(3) as u32,
                    duration_ms: parse(4),
                }),
                None => Err(anyhow::anyhow!("usage: screen2imgur popup <file.mp4> <w> <h> <duration_ms>")),
            }
        }
        Some("selftest") => selftest(&args[1..]),
        Some("imgur") => imgur_cli(&args[1..]),
        _ => tray::run(),
    };
    if let Err(e) = result {
        log(&format!("fatal: {e:#}"));
        #[cfg(debug_assertions)]
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
