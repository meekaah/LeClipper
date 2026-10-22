//! MP4 -> GIF using gifski.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use imgref::ImgVec;
use rgb::RGBA8;

pub struct GifOptions {
    pub fps: u32,
    pub max_width: u32,
    pub quality: u8,
}

#[derive(Default)]
pub struct Progress {
    pub frames_in: AtomicU32,
    pub frames_out: AtomicU32,
    pub cancel: AtomicBool,
}

struct Reporter(Arc<Progress>);

impl gifski::progress::ProgressReporter for Reporter {
    fn increase(&mut self) -> bool {
        self.0.frames_out.fetch_add(1, Ordering::Relaxed);
        !self.0.cancel.load(Ordering::Relaxed)
    }
}

pub fn mp4_to_gif(
    src: &Path,
    dst: &Path,
    crop: (u32, u32),
    trim: Option<(f64, f64)>,
    opts: &GifOptions,
    progress: Arc<Progress>,
) -> Result<()> {
    let width = (opts.max_width > 0 && opts.max_width < crop.0).then_some(opts.max_width);
    let (collector, writer) = gifski::new(gifski::Settings {
        width,
        height: None,
        quality: opts.quality,
        fast: false,
        repeat: gifski::Repeat::Infinite,
    })?;

    let src = src.to_path_buf();
    let step = 1.0 / opts.fps.max(1) as f64;
    let prog = progress.clone();
    let decoder = std::thread::spawn(move || -> Result<()> {
        let mut index = 0usize;
        let mut first: Option<f64> = None;
        let mut next_pts = 0.0f64;
        let on_frame = |f: crate::decode::Frame| {
            if prog.cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let t0 = *first.get_or_insert(f.time_secs);
            let pts = f.time_secs - t0;
            // Keep the frame if it's due; small epsilon absorbs timestamp jitter.
            if index > 0 && pts + step * 0.25 < next_pts {
                return Ok(true);
            }
            let pixels: Vec<RGBA8> = f
                .rgba
                .chunks_exact(4)
                .map(|p| RGBA8::new(p[0], p[1], p[2], p[3]))
                .collect();
            let pts = if index == 0 { 0.0 } else { pts.max(next_pts - step * 0.25) };
            collector.add_frame_rgba(index, ImgVec::new(pixels, f.width as usize, f.height as usize), pts)?;
            index += 1;
            next_pts = pts + step;
            prog.frames_in.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        };
        match trim {
            Some(range) => crate::decode::for_each_frame_trimmed(&src, Some(crop), range, on_frame)?,
            None => crate::decode::for_each_frame(&src, Some(crop), on_frame)?,
        }
        // Dropping the collector tells the writer there are no more frames.
        drop(collector);
        Ok(())
    });

    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let write_result = writer.write(BufWriter::new(file), &mut Reporter(progress.clone()));
    let decode_result = decoder.join().map_err(|_| anyhow!("decoder thread panicked"))?;

    if progress.cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(dst);
        return Err(anyhow!("cancelled"));
    }
    decode_result?;
    write_result?;
    Ok(())
}
