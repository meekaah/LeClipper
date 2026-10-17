//! Looping video preview for the popup. A background thread decodes the recording over and
//! over, already scaled down to preview size, a few frames ahead of playback. Decoding at
//! preview size is many times faster than real time, so the clip plays at its full frame rate
//! without having to hold every frame in memory.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui;

/// Longest preview edge sizes, in pixels.
const MAX_W: f64 = 640.0;
const MAX_H: f64 = 400.0;
/// Frames decoded ahead of playback.
const QUEUE: usize = 8;
/// If playback falls further behind than this (e.g. the window was minimized),
/// it jumps to the frame at hand instead of rushing to catch up.
const MAX_LAG: f64 = 0.25;

enum Msg {
    /// A frame and its play time, which keeps increasing across loops.
    Frame(egui::ColorImage, f64),
    /// The recording could not be decoded at all.
    Failed,
}

pub struct Preview {
    rx: Option<Receiver<Msg>>,
    texture: Option<egui::TextureHandle>,
    next: Option<(egui::ColorImage, f64)>,
    /// Play time of the frame on screen.
    shown_at: Option<f64>,
    failed: bool,
    /// Set while no frame is queued here, so the decoder knows to wake the UI for the next one.
    waiting: Arc<AtomicBool>,
    /// Playback clock: position = now - started.
    started: Instant,
    paused_at: Option<f64>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Preview {
    pub fn start(ctx: &egui::Context, path: PathBuf, crop: (u32, u32), duration_ms: u64) -> Self {
        let (w, h) = (crop.0.max(1) as f64, crop.1.max(1) as f64);
        let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
        let size = (((w * scale).round() as usize).max(1), ((h * scale).round() as usize).max(1));
        let duration = duration_ms as f64 / 1000.0;

        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let cancel = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(true));
        let stop = cancel.clone();
        let wake = waiting.clone();
        let ctx2 = ctx.clone();
        let worker =
            std::thread::spawn(move || decode_loop(&path, crop, size, duration, &tx, &stop, &wake, &ctx2));

        Self {
            rx: Some(rx),
            texture: None,
            next: None,
            shown_at: None,
            failed: false,
            waiting,
            started: Instant::now(),
            paused_at: None,
            cancel,
            worker: Some(worker),
        }
    }

    /// True until the first frame is available.
    pub fn loading(&self) -> bool {
        self.texture.is_none() && !self.failed
    }

    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn toggle_pause(&mut self) {
        match self.paused_at.take() {
            Some(pos) => self.started = Instant::now() - Duration::from_secs_f64(pos),
            None => self.paused_at = Some(self.position()),
        }
    }

    fn position(&self) -> f64 {
        self.paused_at.unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    /// Restarts the clock so that `pos` is now.
    fn seek(&mut self, pos: f64) {
        match &mut self.paused_at {
            Some(p) => *p = pos,
            None => self.started = Instant::now() - Duration::from_secs_f64(pos.max(0.0)),
        }
    }

    /// The frame to show now. Schedules the repaint for the next frame change.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        let mut shown = None;
        loop {
            if self.next.is_none() {
                match self.rx.as_ref().map(|rx| rx.try_recv()) {
                    Some(Ok(Msg::Frame(img, t))) => self.next = Some((img, t)),
                    Some(Ok(Msg::Failed)) => self.failed = true,
                    _ => {}
                }
            }
            let Some((_, t)) = &self.next else { break };
            let t = *t;
            // egui wakes up about a display frame ahead of the requested time. Showing a frame
            // that is due within half a frame avoids repainting again just to wait for it.
            let early = self.shown_at.map_or(0.0, |s| (t - s) / 2.0);
            if self.texture.is_none() && shown.is_none() {
                // Start the clock at the first frame, however long decoding took to get going.
                self.seek(t);
            }
            if t > self.position() + early {
                break;
            }
            shown = self.next.take();
            self.shown_at = Some(t);
        }
        if let Some((img, t)) = shown {
            if self.position() - t > MAX_LAG {
                self.seek(t);
            }
            match &mut self.texture {
                Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
        }
        match &self.next {
            Some((_, t)) => {
                if self.paused_at.is_none() {
                    // egui wakes up a predicted frame time early; we already allow for that.
                    let dt = ctx.input(|i| i.predicted_dt) as f64;
                    ctx.request_repaint_after(Duration::from_secs_f64((t - self.position()).max(0.0) + dt));
                }
            }
            None => {
                // The decoder wakes us when it delivers the next frame.
                self.waiting.store(true, Ordering::Relaxed);
                if let Some(Ok(Msg::Frame(img, t))) = self.rx.as_ref().map(|rx| rx.try_recv()) {
                    // It arrived just now, possibly before the flag was set.
                    self.next = Some((img, t));
                    ctx.request_repaint();
                }
            }
        }
        self.texture.as_ref()
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // Stop decoding so the recording file is closed before the popup deletes it.
        // Dropping the receiver also unblocks the decoder if it is waiting on a full queue.
        self.cancel.store(true, Ordering::Relaxed);
        self.rx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Decodes the recording on a loop, sending frames stamped with their play time.
fn decode_loop(
    path: &Path,
    crop: (u32, u32),
    (pw, ph): (usize, usize),
    duration: f64,
    tx: &SyncSender<Msg>,
    stop: &AtomicBool,
    waiting: &AtomicBool,
    ctx: &egui::Context,
) {
    // Timestamps restart with every pass; `offset` is where the current pass starts playing.
    let mut offset = 0.0f64;
    let mut first: Option<f64> = None;
    let mut last = 0.0f64;
    let mut step = 1.0 / 60.0;
    // Also read between passes, while the frame callback holds its borrows.
    let sent = Cell::new(0usize);
    let mut on_frame = |f: crate::decode::Frame| -> anyhow::Result<bool> {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let start = *first.get_or_insert(f.time_secs);
        let mut pts = f.time_secs - start;
        if pts < last {
            // Started over.
            offset += duration.max(last + step);
            first = Some(f.time_secs);
            pts = 0.0;
        } else if pts > last {
            step = pts - last;
        }
        last = pts;
        let img = if f.width as usize == pw && f.height as usize == ph {
            egui::ColorImage::from_rgba_unmultiplied([pw, ph], &f.rgba)
        } else {
            let small = downscale(&f.rgba, f.width as usize, f.height as usize, pw, ph);
            egui::ColorImage::from_rgba_unmultiplied([pw, ph], &small)
        };
        if tx.send(Msg::Frame(img, offset + pts)).is_err() {
            return Ok(false);
        }
        sent.set(sent.get() + 1);
        if waiting.swap(false, Ordering::Relaxed) {
            ctx.request_repaint();
        }
        Ok(true)
    };

    let r = crate::decode::for_each_frame_scaled(path, (pw as u32, ph as u32), true, &mut on_frame);
    if r.is_err() && !stop.load(Ordering::Relaxed) {
        // Fall back to decoding full size and shrinking the frames here.
        loop {
            let before = sent.get();
            let r = crate::decode::for_each_frame(path, Some(crop), &mut on_frame);
            if r.is_err() || stop.load(Ordering::Relaxed) || sent.get() == before {
                break;
            }
        }
    }
    if sent.get() == 0 && !stop.load(Ordering::Relaxed) {
        let _ = tx.send(Msg::Failed);
        ctx.request_repaint();
    }
}

/// Area-average downscale of packed RGBA.
fn downscale(src: &[u8], w: usize, h: usize, dw: usize, dh: usize) -> Vec<u8> {
    if dw == w && dh == h {
        return src.to_vec();
    }
    let spans = |n: usize, dn: usize| -> Vec<(usize, usize)> {
        (0..dn)
            .map(|i| {
                let a = i * n / dn;
                (a, ((i + 1) * n / dn).max(a + 1).min(n))
            })
            .collect()
    };
    let xs = spans(w, dw);
    let ys = spans(h, dh);
    let mut out = vec![0u8; dw * dh * 4];
    for (dy, &(y0, y1)) in ys.iter().enumerate() {
        for (dx, &(x0, x1)) in xs.iter().enumerate() {
            let mut acc = [0u32; 4];
            for y in y0..y1 {
                let row = &src[(y * w + x0) * 4..(y * w + x1) * 4];
                for p in row.chunks_exact(4) {
                    acc[0] += p[0] as u32;
                    acc[1] += p[1] as u32;
                    acc[2] += p[2] as u32;
                    acc[3] += p[3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (dy * dw + dx) * 4;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    out
}
