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
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.context("MFCreateAttributes")?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

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
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            // Positive stride = top-down rows (RGB defaults to bottom-up otherwise).
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
            writer
                .SetInputMediaType(stream, &input, None)
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

    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize() }.context("Finalize MP4")
    }
}
