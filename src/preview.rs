//! Looping video preview for the popup: the recording is decoded once in the background
//! into small frames (bounded by a memory budget) that are then played back on a loop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui;

/// Longest preview edge sizes, in pixels.
const MAX_W: f64 = 640.0;
const MAX_H: f64 = 400.0;
const MAX_FPS: f64 = 30.0;
/// Total pixel memory the decoded preview may use.
const BUDGET_BYTES: f64 = 192.0 * 1024.0 * 1024.0;

enum Msg {
    Frame(egui::ColorImage, f64),
    Done,
}

pub struct Preview {
    rx: Receiver<Msg>,
    frames: Vec<(egui::TextureHandle, f64)>,
    done: bool,
    step: f64,
    duration: f64,
    /// Playback clock: position = now - started (mod loop length).
    started: Instant,
    paused_at: Option<f64>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Preview {
    pub fn start(ctx: &egui::Context, path: PathBuf, crop: (u32, u32), duration_ms: u64) -> Self {
        let (w, h) = (crop.0.max(1) as f64, crop.1.max(1) as f64);
        let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
        let (pw, ph) = (((w * scale).round() as usize).max(1), ((h * scale).round() as usize).max(1));
        let duration = (duration_ms as f64 / 1000.0).max(0.001);
        let max_frames = BUDGET_BYTES / (pw * ph * 4) as f64;
        let fps = (max_frames / duration).clamp(1.0, MAX_FPS);
        let step = 1.0 / fps;

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let ctx2 = ctx.clone();
        let worker = std::thread::spawn(move || {
            let mut next_pts = 0.0f64;
            let mut first: Option<f64> = None;
            let _ = crate::decode::for_each_frame(&path, Some(crop), |f| {
                if stop.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                let pts = f.time_secs - *first.get_or_insert(f.time_secs);
                if pts + step * 0.25 < next_pts {
                    return Ok(true);
                }
                next_pts = pts + step;
                let small = downscale(&f.rgba, f.width as usize, f.height as usize, pw, ph);
                let img = egui::ColorImage::from_rgba_unmultiplied([pw, ph], &small);
                let _ = tx.send(Msg::Frame(img, pts));
                ctx2.request_repaint();
                Ok(true)
            });
            let _ = tx.send(Msg::Done);
            ctx2.request_repaint();
        });

        Self {
            rx,
            frames: Vec::new(),
            done: false,
            step,
            duration,
            started: Instant::now(),
            paused_at: None,
            cancel,
            worker: Some(worker),
        }
    }

    /// True until the first frame is available.
    pub fn loading(&self) -> bool {
        self.frames.is_empty() && !self.done
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

    fn loop_len(&self) -> f64 {
        let last = self.frames.last().map_or(0.0, |f| f.1);
        self.duration.max(last + self.step)
    }

    fn position(&self) -> f64 {
        if let Some(p) = self.paused_at {
            return p;
        }
        let t = self.started.elapsed().as_secs_f64();
        if self.done {
            t % self.loop_len()
        } else {
            // Still decoding: play up to what's ready and hold there.
            t.min(self.frames.last().map_or(0.0, |f| f.1))
        }
    }

    /// The frame to show now. Schedules the repaint for the next frame change.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Frame(img, pts) => {
                    let name = format!("preview{}", self.frames.len());
                    self.frames.push((ctx.load_texture(name, img, egui::TextureOptions::LINEAR), pts));
                }
                Msg::Done => self.done = true,
            }
        }
        if self.frames.is_empty() {
            return None;
        }
        let t = self.position();
        let i = self.frames.partition_point(|f| f.1 <= t).saturating_sub(1);
        if self.paused_at.is_none() {
            let next = self.frames.get(i + 1).map_or(self.loop_len(), |f| f.1);
            ctx.request_repaint_after(Duration::from_secs_f64((next - t).max(0.005)));
        }
        Some(&self.frames[i].0)
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // Stop decoding so the recording file is closed before the popup deletes it.
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
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
