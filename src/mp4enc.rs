//! H.264/MP4 encoding through the Media Foundation sink writer.
//! Input is top-down BGRA; MF inserts the colour converter and picks a hardware encoder when available.

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;

pub struct Mp4Writer {
    writer: IMFSinkWriter,
    stream: u32,
    width: u32,
    height: u32,
}

pub fn mf_startup() -> Result<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

impl Mp4Writer {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        Self::create_with(path, width, height, fps, bitrate, None)
    }

    /// With `gpu`, the input is NV12 surfaces on that device (see [`Mp4Writer::write_sample`])
    /// instead of BGRA in memory.
    fn create_with(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        gpu: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self> {
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 3)?;
            let attrs = attrs.context("MFCreateAttributes")?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            if let Some(gpu) = gpu {
                attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, gpu)?;
            }

            let url = HSTRING::from(path.as_os_str());
            let writer = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, &attrs)
                .context("MFCreateSinkWriterFromURL")?;

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            let stream = writer.AddStream(&out).context("AddStream (H.264)")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, if gpu.is_some() { &MFVideoFormat_NV12 } else { &MFVideoFormat_RGB32 })?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            if gpu.is_none() {
                // Positive stride = top-down rows (RGB defaults to bottom-up otherwise).
                input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
            }
            let mut params: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut params, 1)?;
            let params = params.context("MFCreateAttributes")?;
            // A key frame every second keeps seeking quick when the popup trims the clip.
            params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, fps)?;
            writer
                .SetInputMediaType(stream, &input, &params)
                .context("SetInputMediaType (BGRA -> H.264)")?;

            writer.BeginWriting().context("BeginWriting")?;
            Ok(Self { writer, stream, width, height })
        }
    }

    /// `src` is BGRA with `pitch` bytes per row. Times are in 100 ns units.
    pub fn write(&mut self, src: *const u8, pitch: usize, time: i64, duration: i64) -> Result<()> {
        let row = self.width as usize * 4;
        let len = row * self.height as usize;
        unsafe {
            let buf = MFCreateMemoryBuffer(len as u32)?;
            let mut dst: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * row), row);
            }
            buf.Unlock()?;
            buf.SetCurrentLength(len as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(self.stream, &sample)?;
        }
        Ok(())
    }

    /// Writes a decoded sample as is. Times are in 100 ns units.
    fn write_sample(&mut self, sample: &IMFSample, time: i64, duration: i64) -> Result<()> {
        unsafe {
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(self.stream, sample)?;
        }
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize() }.context("Finalize MP4")
    }
}

/// Re-encodes what `src` shows from `start` to `end` seconds into `dst`.
pub fn trim(src: &Path, dst: &Path, crop: (u32, u32), range: (f64, f64), fps: u32, bitrate: u32) -> Result<()> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    mf_startup()?;
    match trim_gpu(src, dst, crop, range, fps, bitrate) {
        Ok(()) => Ok(()),
        Err(e) => {
            crate::log(&format!("GPU trim failed, trimming on the CPU: {e:#}"));
            let _ = std::fs::remove_file(dst);
            trim_cpu(src, dst, crop, range, fps, bitrate)
        }
    }
}

/// Decodes and re-encodes on the GPU: the frames stay there as NV12 from decoder to encoder,
/// which is many times faster than converting every frame to RGB and back on the CPU.
fn trim_gpu(src: &Path, dst: &Path, crop: (u32, u32), (start, end): (f64, f64), fps: u32, bitrate: u32) -> Result<()> {
    let gpu = unsafe { crate::decode::dxgi_manager()? };
    let mut reader = crate::decode::Reader::open_nv12(src, crop, &gpu)?;
    let mut writer = Mp4Writer::create_with(dst, crop.0, crop.1, fps, bitrate, Some(&gpu))?;
    let ticks = |secs: f64| (secs * 10_000_000.0).round() as i64;
    if start > 0.0 {
        reader.seek(start)?;
    }
    // Same cut as `decode::for_each_frame_trimmed`: the frame on screen at `start` comes first.
    // Each frame is written once the next one says how long it stays on screen.
    let mut prev: Option<(crate::decode::Sample, f64)> = None;
    while let Some(s) = reader.next_sample()? {
        if s.time_secs >= end {
            break;
        }
        let t = (s.time_secs - start).max(0.0);
        if let Some((p, pt)) = prev.take() {
            if t > 0.0 {
                writer.write_sample(p.sample(), ticks(pt), (ticks(t) - ticks(pt)).max(1))?;
            }
        }
        prev = Some((s, t));
    }
    let (p, pt) = prev.context("nothing to keep")?;
    writer.write_sample(p.sample(), ticks(pt), (ticks(end - start) - ticks(pt)).max(1))?;
    writer.finish()
}

fn trim_cpu(src: &Path, dst: &Path, crop: (u32, u32), (start, end): (f64, f64), fps: u32, bitrate: u32) -> Result<()> {
    let ticks = |secs: f64| (secs * 10_000_000.0).round() as i64;
    let mut writer: Option<Mp4Writer> = None;
    let mut bgra = Vec::new();
    // Each frame is written once the next one says how long it stays on screen.
    let mut prev: Option<crate::decode::Frame> = None;
    let mut put = |writer: &mut Option<Mp4Writer>, f: &crate::decode::Frame, until: f64| -> Result<()> {
        let w = match writer {
            Some(w) => w,
            None => writer.insert(Mp4Writer::create(dst, f.width, f.height, fps, bitrate)?),
        };
        bgra.clear();
        bgra.extend(f.rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]));
        let t = ticks(f.time_secs);
        w.write(bgra.as_ptr(), f.width as usize * 4, t, (ticks(until) - t).max(1))
    };
    crate::decode::for_each_frame_trimmed(src, Some(crop), (start, end), |f| {
        if let Some(p) = prev.replace(f) {
            put(&mut writer, &p, prev.as_ref().map_or(0.0, |f| f.time_secs))?;
        }
        Ok(true)
    })?;
    let last = prev.context("nothing to keep")?;
    put(&mut writer, &last, (end - start).max(last.time_secs))?;
    writer.context("nothing to keep")?.finish()
}
