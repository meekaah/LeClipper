//! Looping video preview for the popup. A background thread decodes the recording over and
//! over, already scaled down to preview size, a few frames ahead of playback. Decoding at
//! preview size is many times faster than real time, so the clip plays at its full frame rate
//! without having to hold every frame in memory.
//!
//! Playback loops over the trimmed part of the clip, and while a trim knob is dragged the
//! preview shows the single frame under it instead.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::decode::{Reader, Sample};

/// Longest preview edge sizes, in pixels.
const MAX_W: f64 = 640.0;
const MAX_H: f64 = 400.0;
/// Frames decoded ahead of playback.
const QUEUE: usize = 8;
/// If playback falls further behind than this (e.g. the window was minimized),
/// it jumps to the frame at hand instead of rushing to catch up.
const MAX_LAG: f64 = 0.25;
/// A target less than this far ahead is reached by decoding on rather than seeking.
const DECODE_AHEAD: f64 = 1.0;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Loop `start..end` (seconds into the clip), the first time round from `from`.
    Play { start: f64, end: f64, from: f64 },
    /// Show the frame on screen at this time, and wait.
    Still(f64),
}

/// What the UI wants the decoder to do. `gen` goes up with every change.
struct Request {
    gen: u64,
    mode: Mode,
    stop: bool,
}

struct Control {
    request: Mutex<Request>,
    changed: Condvar,
}

enum Msg {
    /// A frame, its play time (which keeps increasing across loops), its time in the clip,
    /// and the request it answers, which may be for a still.
    Frame { img: egui::ColorImage, at: f64, clip: f64, gen: u64, still: bool },
    /// The recording could not be decoded at all.
    Failed,
}

pub struct Preview {
    rx: Option<Receiver<Msg>>,
    texture: Option<egui::TextureHandle>,
    /// The next frame: image, play time and clip time.
    next: Option<(egui::ColorImage, f64, f64)>,
    /// Play time and clip time of the frame on screen.
    shown_at: Option<f64>,
    shown_clip: Option<f64>,
    failed: bool,
    /// Set while no frame is queued here, so the decoder knows to wake the UI for the next one.
    waiting: Arc<AtomicBool>,
    /// Playback clock: position = now - started.
    started: Instant,
    paused_at: Option<f64>,
    control: Arc<Control>,
    gen: u64,
    mode: Mode,
    /// The clock restarts at the first frame of a new request.
    resync: bool,
    worker: Option<JoinHandle<()>>,
}

impl Preview {
    pub fn start(ctx: &egui::Context, path: PathBuf, crop: (u32, u32), duration_ms: u64) -> Self {
        let (w, h) = (crop.0.max(1) as f64, crop.1.max(1) as f64);
        let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
        let size = (((w * scale).round() as usize).max(1), ((h * scale).round() as usize).max(1));
        let mode = Mode::Play { start: 0.0, end: duration_ms as f64 / 1000.0, from: 0.0 };

        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let control =
            Arc::new(Control { request: Mutex::new(Request { gen: 1, mode, stop: false }), changed: Condvar::new() });
        let waiting = Arc::new(AtomicBool::new(true));
        let ctl = control.clone();
        let wake = waiting.clone();
        let ctx2 = ctx.clone();
        let worker = std::thread::spawn(move || decode_loop(&path, crop, size, &ctl, &tx, &wake, &ctx2));

        Self {
            rx: Some(rx),
            texture: None,
            next: None,
            shown_at: None,
            shown_clip: None,
            failed: false,
            waiting,
            started: Instant::now(),
            paused_at: None,
            control,
            gen: 1,
            mode,
            resync: true,
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

    /// Loops `start..end` seconds of the clip, the first time round from `from`.
    pub fn play(&mut self, start: f64, end: f64, from: f64) {
        self.request(Mode::Play { start, end, from: from.clamp(start, end) });
    }

    /// Shows the frame on screen `t` seconds into the clip, until told otherwise.
    pub fn show_at(&mut self, t: f64) {
        if self.mode != Mode::Still(t) {
            self.request(Mode::Still(t));
        }
    }

    /// Where playback is in the clip, in seconds, while it plays.
    pub fn playhead(&self) -> Option<f64> {
        let Mode::Play { end, .. } = self.mode else { return None };
        if self.resync {
            return None;
        }
        let (at, clip) = (self.shown_at?, self.shown_clip?);
        Some((clip + (self.position() - at).clamp(0.0, MAX_LAG)).min(end))
    }

    fn request(&mut self, mode: Mode) {
        {
            let mut r = self.control.request.lock().unwrap();
            r.gen += 1;
            r.mode = mode;
            self.gen = r.gen;
        }
        self.control.changed.notify_all();
        let was_still = matches!(self.mode, Mode::Still(_));
        self.mode = mode;
        self.resync = true;
        // Play times start over with each request; the old one would hold back the next frame.
        self.shown_at = None;
        self.waiting.store(true, Ordering::Relaxed);
        // Unblock the decoder if it waits on a full queue. Only the latest still is worth keeping.
        let mut latest = self.next.take().filter(|_| was_still);
        while let Some(f) = self.recv() {
            latest = Some(f);
        }
        self.next = latest.filter(|_| matches!(mode, Mode::Still(_)));
    }

    /// The next frame from the decoder for the current request, skipping stale ones. Stills
    /// for earlier spots still show while a knob is dragged, so the preview keeps up with it.
    fn recv(&mut self) -> Option<(egui::ColorImage, f64, f64)> {
        let showing_still = matches!(self.mode, Mode::Still(_));
        loop {
            match self.rx.as_ref()?.try_recv() {
                Ok(Msg::Frame { img, at, clip, gen, still }) if gen == self.gen || still && showing_still => {
                    return Some((img, at, clip))
                }
                Ok(Msg::Frame { .. }) => {}
                Ok(Msg::Failed) => self.failed = true,
                Err(_) => return None,
            }
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
                self.next = self.recv();
            }
            let Some((_, t, _)) = &self.next else { break };
            let t = *t;
            // egui wakes up about a display frame ahead of the requested time. Showing a frame
            // that is due within half a frame avoids repainting again just to wait for it.
            let early = self.shown_at.map_or(0.0, |s| (t - s) / 2.0);
            if self.resync {
                // Start the clock at the request's first frame, however long decoding took.
                self.seek(t);
                self.resync = false;
            }
            if t > self.position() + early {
                break;
            }
            shown = self.next.take();
            self.shown_at = Some(t);
        }
        if let Some((img, t, clip)) = shown {
            if self.position() - t > MAX_LAG {
                self.seek(t);
            }
            self.shown_clip = Some(clip);
            match &mut self.texture {
                Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
        }
        match &self.next {
            Some((_, t, _)) => {
                if self.paused_at.is_none() {
                    // egui wakes up a predicted frame time early; we already allow for that.
                    let dt = ctx.input(|i| i.predicted_dt) as f64;
                    ctx.request_repaint_after(Duration::from_secs_f64((t - self.position()).max(0.0) + dt));
                }
            }
            None => {
                // The decoder wakes us when it delivers the next frame.
                self.waiting.store(true, Ordering::Relaxed);
                if let Some(next) = self.recv() {
                    // It arrived just now, possibly before the flag was set.
                    self.next = Some(next);
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
        self.control.request.lock().unwrap().stop = true;
        self.control.changed.notify_all();
        self.rx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Opens the recording at preview size, or failing that full size (shrunk here), and reads
/// the first frame to make sure it decodes.
fn open(path: &Path, crop: (u32, u32), (pw, ph): (usize, usize)) -> Option<(Reader, Sample)> {
    let size = (pw as u32, ph as u32);
    [(size, Some(size)), (crop, None)].into_iter().find_map(|(crop, scale)| {
        let mut r = Reader::open(path, Some(crop), scale).ok()?;
        let s = r.next_sample().ok()??;
        r.pixels(&s).ok()?;
        Some((r, s))
    })
}

/// Decodes what the UI asks for (see [`Mode`]), sending frames stamped with their play time.
fn decode_loop(
    path: &Path,
    crop: (u32, u32),
    size: (usize, usize),
    control: &Control,
    tx: &SyncSender<Msg>,
    waiting: &AtomicBool,
    ctx: &egui::Context,
) {
    let fail = || {
        let _ = tx.send(Msg::Failed);
        ctx.request_repaint();
    };
    let Some((mut reader, first)) = open(path, crop, size) else { return fail() };
    // A frame read but not yet dealt with.
    let mut pending = Some(first);
    // The latest frame dealt with. Only frames that are shown get their pixels copied out,
    // which makes skipping ahead after a seek many times faster.
    let mut last: Option<Sample> = None;
    let mut gen = 0;
    let mut mode = Mode::Still(0.0);
    // While set, frames up to this clip time are skipped, and the last of them shown.
    let mut seeking: Option<f64> = None;
    // Play time of the loop's start in the current pass.
    let mut offset = 0.0;
    let mut idle = false;
    let mut sent = false;

    loop {
        // A seek under way is finished first: while a knob is dragged, requests come faster than
        // seeks, and dropping each one for the next would show nothing until the drag ends.
        let changed = if seeking.is_some() {
            None
        } else {
            let mut r = control.request.lock().unwrap();
            while idle && !r.stop && r.gen == gen {
                r = control.changed.wait(r).unwrap();
            }
            if r.stop {
                return;
            }
            (r.gen != gen).then_some((r.gen, r.mode))
        };
        if let Some((g, m)) = changed {
            (gen, mode, offset, idle) = (g, m, 0.0, false);
            let target = match m {
                Mode::Play { from, .. } => from,
                Mode::Still(t) => t,
            };
            let ahead = last.as_ref().is_some_and(|f| f.time_secs <= target && target - f.time_secs < DECODE_AHEAD);
            if !ahead {
                if reader.seek(target).is_err() {
                    return;
                }
                (last, pending) = (None, None);
            }
            seeking = Some(target);
        }

        let next = match pending.take() {
            Some(f) => Some(f),
            None => match reader.next_sample() {
                Ok(f) => f,
                Err(_) => {
                    if !sent {
                        fail();
                    }
                    return;
                }
            },
        };

        if let Some(target) = seeking {
            match next {
                Some(f) if f.time_secs <= target => last = Some(f),
                next => {
                    seeking = None;
                    pending = next;
                    // On screen at `target`: the last frame at or before it, or the first after it
                    // if the seek overshot.
                    let Some(f) = last.take().or_else(|| pending.take()) else {
                        idle = true;
                        continue;
                    };
                    let at = match mode {
                        Mode::Play { start, .. } => offset + target - start,
                        Mode::Still(_) => 0.0,
                    };
                    if !send(tx, waiting, ctx, &reader, &f, size, at, target, gen, matches!(mode, Mode::Still(_))) {
                        return;
                    }
                    sent = true;
                    last = Some(f);
                    idle = matches!(mode, Mode::Still(_));
                }
            }
            continue;
        }

        let Mode::Play { start, end, .. } = mode else {
            idle = true;
            continue;
        };
        match next {
            Some(f) if f.time_secs < end => {
                if !send(tx, waiting, ctx, &reader, &f, size, offset + f.time_secs - start, f.time_secs, gen, false) {
                    return;
                }
                last = Some(f);
            }
            _ => {
                // Past the end: loop back to the start.
                offset += end - start;
                if reader.seek(start).is_err() {
                    return;
                }
                (last, pending) = (None, None);
                seeking = Some(start);
            }
        }
    }
}

fn send(
    tx: &SyncSender<Msg>,
    waiting: &AtomicBool,
    ctx: &egui::Context,
    reader: &Reader,
    sample: &Sample,
    (pw, ph): (usize, usize),
    at: f64,
    clip: f64,
    gen: u64,
    still: bool,
) -> bool {
    let Ok(f) = reader.pixels(sample) else { return false };
    let img = if f.width as usize == pw && f.height as usize == ph {
        egui::ColorImage::from_rgba_unmultiplied([pw, ph], &f.rgba)
    } else {
        let small = downscale(&f.rgba, f.width as usize, f.height as usize, pw, ph);
        egui::ColorImage::from_rgba_unmultiplied([pw, ph], &small)
    };
    if tx.send(Msg::Frame { img, at, clip, gen, still }).is_err() {
        return false;
    }
    if waiting.swap(false, Ordering::Relaxed) {
        ctx.request_repaint();
    }
    true
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
