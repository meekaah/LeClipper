//! Decodes the recorded MP4 back into RGBA frames (for GIF export and the preview thumbnail).

use std::path::Path;

use anyhow::{Context, Result};
use windows::core::{Interface, GUID, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// RGBA, top-down, tightly packed.
    pub rgba: Vec<u8>,
    pub time_secs: f64,
}

const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Calls `on_frame` for every decoded frame until it returns `false` or the stream ends.
/// `crop` is the real recorded size; decoders may pad the frame to a multiple of 16.
pub fn for_each_frame(
    path: &Path,
    crop: Option<(u32, u32)>,
    on_frame: impl FnMut(Frame) -> Result<bool>,
) -> Result<()> {
    read_frames(path, crop, None, false, on_frame)
}

/// Like [`for_each_frame`], but Media Foundation decodes and scales the frames to `size`
/// on the GPU when it can, which is far cheaper than converting full-size frames and
/// shrinking them afterwards. With `repeat`, it starts over at the end of the stream
/// (timestamps restart too) until `on_frame` returns `false`.
pub fn for_each_frame_scaled(
    path: &Path,
    size: (u32, u32),
    repeat: bool,
    on_frame: impl FnMut(Frame) -> Result<bool>,
) -> Result<()> {
    read_frames(path, Some(size), Some(size), repeat, on_frame)
}

fn read_frames(
    path: &Path,
    crop: Option<(u32, u32)>,
    scale_to: Option<(u32, u32)>,
    repeat: bool,
    mut on_frame: impl FnMut(Frame) -> Result<bool>,
) -> Result<()> {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
        crate::mp4enc::mf_startup()?;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.context("MFCreateAttributes")?;
        // Kept alive for as long as the reader uses it.
        let mut _gpu: Option<IMFDXGIDeviceManager> = None;
        if scale_to.is_some() {
            // Only the advanced video processor can resize. With a D3D device it decodes and
            // scales on the GPU; without one it falls back to (multithreaded) software.
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            if let Ok(manager) = dxgi_manager() {
                attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
                _gpu = Some(manager);
            }
        } else {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        }

        let url = HSTRING::from(path.as_os_str());
        let reader = MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &attrs)
            .context("open recording for decoding")?;

        let want = MFCreateMediaType()?;
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        if let Some((sw, sh)) = scale_to {
            want.SetUINT64(&MF_MT_FRAME_SIZE, (sw as u64) << 32 | sh as u64)?;
        }
        reader
            .SetCurrentMediaType(FIRST_VIDEO, None, &want)
            .context("decoder cannot output RGB32")?;

        let cur = reader.GetCurrentMediaType(FIRST_VIDEO)?;
        let size = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (fw, fh) = ((size >> 32) as u32, size as u32);
        let default_stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|v| v as i32).unwrap_or(fw as i32 * 4);
        let (w, h) = match crop {
            Some((cw, ch)) => (cw.min(fw), ch.min(fh)),
            None => (fw, fh),
        };

        let mut any = false;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                if !repeat || !any {
                    break;
                }
                // Rewinding keeps the decoder, which is much faster than reopening the file.
                reader.SetCurrentPosition(&GUID::zeroed(), &start_position())?;
                continue;
            }
            let Some(sample) = sample else { continue };
            any = true;
            let buf = sample.ConvertToContiguousBuffer()?;

            let mut rgba = vec![0u8; (w * h * 4) as usize];
            // Prefer the 2D lock: it reports the real pitch, which is negative for bottom-up images.
            if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                buf2d.Lock2D(&mut scan0, &mut pitch)?;
                copy_rows(scan0, pitch as isize, w, h, &mut rgba);
                buf2d.Unlock2D()?;
            } else {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len))?;
                // The media type reports the cropped size, but the buffer may be padded
                // (e.g. 1002x562 arrives as 1008x576), so derive the real pitch from its length.
                let row = default_stride.unsigned_abs() as usize;
                let pitch = real_pitch(len as usize, row, fw as usize, fh as usize);
                let rows = (len as usize / pitch) as isize;
                let (start, pitch) = if default_stride < 0 {
                    (ptr.offset(pitch as isize * (rows - 1)), -(pitch as isize))
                } else {
                    (ptr, pitch as isize)
                };
                copy_rows(start, pitch, w, h, &mut rgba);
                buf.Unlock()?;
            }

            let frame = Frame { width: w, height: h, rgba, time_secs: ts as f64 / 10_000_000.0 };
            if !on_frame(frame)? {
                break;
            }
        }
    }
    Ok(())
}

/// Position 0 in the source reader's default time format (100 ns units).
fn start_position() -> windows::Win32::System::Com::StructuredStorage::PROPVARIANT {
    use windows::Win32::System::Com::StructuredStorage::*;
    let mut pos = PROPVARIANT::default();
    // SAFETY: plain-data union; VT_I8 with hVal = 0.
    unsafe {
        (*pos.Anonymous.Anonymous).vt = windows::Win32::System::Variant::VT_I8;
        (*pos.Anonymous.Anonymous).Anonymous.hVal = 0;
    }
    pos
}

/// A Media Foundation device manager around a new hardware D3D11 device.
unsafe fn dxgi_manager() -> Result<IMFDXGIDeviceManager> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;

    let mut device: Option<ID3D11Device> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )?;
    let device = device.context("no D3D11 device")?;
    // Media Foundation uses the device from several threads.
    let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
    let manager = manager.context("MFCreateDXGIDeviceManager")?;
    manager.ResetDevice(&device, token)?;
    Ok(manager)
}

/// Bytes per row of a possibly padded RGB32 buffer of `len` bytes holding at least `w`x`h` pixels.
fn real_pitch(len: usize, stride: usize, w: usize, h: usize) -> usize {
    if len == stride * h {
        return stride;
    }
    for align in [16, 32, 64, 128, 256] {
        let pitch = w.div_ceil(align) * align * 4;
        if len % pitch == 0 && len / pitch >= h {
            return pitch;
        }
    }
    (len / h.max(1)).max(stride)
}

/// BGRX rows starting at `scan0` with signed `pitch` -> packed RGBA (opaque).
unsafe fn copy_rows(scan0: *const u8, pitch: isize, w: u32, h: u32, out: &mut [u8]) {
    let row = w as usize * 4;
    for y in 0..h as usize {
        let src = std::slice::from_raw_parts(scan0.offset(pitch * y as isize), row);
        let dst = &mut out[y * row..(y + 1) * row];
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
}
