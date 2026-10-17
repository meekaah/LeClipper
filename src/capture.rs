//! Region recording: Windows.Graphics.Capture on one monitor, GPU crop into a staging
//! texture, paced to the target fps and fed to the MP4 writer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::config::Config;
use crate::mp4enc::{self, Mp4Writer};

/// A capture rectangle on a single monitor, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    /// Raw HMONITOR (kept as an integer so the struct is `Send`).
    pub monitor: isize,
    /// Absolute virtual-screen coordinates.
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Top-left of the monitor in virtual-screen coordinates.
    pub mon_x: i32,
    pub mon_y: i32,
}

pub struct RecordingResult {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

pub struct Recording {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<RecordingResult>>,
}

impl Recording {
    /// Signals the capture thread and waits for the file to be finalized.
    pub fn stop(self) -> Result<RecordingResult> {
        self.stop.store(true, Ordering::SeqCst);
        match self.handle.join() {
            Ok(r) => r,
            Err(_) => bail!("capture thread panicked"),
        }
    }
}

struct Settings {
    region: Region,
    fps: u32,
    bitrate: u32,
    cursor: bool,
    path: PathBuf,
}

pub fn start(region: Region, cfg: &Config) -> Result<Recording> {
    std::fs::create_dir_all(crate::config::temp_dir())?;
    let name = format!("rec_{}.mp4", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let settings = Settings {
        region,
        fps: cfg.video_fps,
        bitrate: cfg.video_bitrate(region.w, region.h),
        cursor: cfg.capture_cursor,
        path: crate::config::temp_dir().join(name),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (init_tx, init_rx) = mpsc::channel::<Result<()>>();
    let stop2 = stop.clone();
    let handle = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run(settings, stop2, init_tx))?;
    match init_rx.recv() {
        Ok(Ok(())) => Ok(Recording { stop, handle }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => match handle.join() {
            Ok(Err(e)) => Err(e),
            _ => bail!("capture thread exited during startup"),
        },
    }
}

struct Pipeline {
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    writer: Mp4Writer,
    frame_rx: mpsc::Receiver<()>,
    src_box: D3D11_BOX,
}

fn setup(s: &Settings) -> Result<Pipeline> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        mp4enc::mf_startup()?;

        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .context("D3D11CreateDevice")?;
        let device = device.context("no D3D11 device")?;
        let ctx = ctx.context("no D3D11 context")?;

        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;

        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop
            .CreateForMonitor(HMONITOR(s.region.monitor as _))
            .context("CreateForMonitor")?;
        let size = item.Size()?;

        let r = &s.region;
        let left = (r.x - r.mon_x).max(0) as u32;
        let top = (r.y - r.mon_y).max(0) as u32;
        if left + r.w > size.Width as u32 || top + r.h > size.Height as u32 {
            bail!(
                "region {}x{}+{}+{} is outside the monitor ({}x{})",
                r.w, r.h, left, top, size.Width, size.Height
            );
        }
        let src_box = D3D11_BOX { left, top, front: 0, right: left + r.w, bottom: top + r.h, back: 1 };

        let desc = D3D11_TEXTURE2D_DESC {
            Width: r.w,
            Height: r.h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.context("staging texture")?;

        let writer = Mp4Writer::create(&s.path, r.w, r.h, s.fps, s.bitrate)?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let (tx, frame_rx) = mpsc::channel::<()>();
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |_, _| {
                let _ = tx.send(());
                Ok(())
            },
        ))?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(s.cursor)?;
        // Windows 11 only; on Windows 10 the yellow border stays (it is never part of the image).
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;

        Ok(Pipeline { ctx, staging, pool, session, writer, frame_rx, src_box })
    }
}

fn run(s: Settings, stop: Arc<AtomicBool>, init_tx: mpsc::Sender<Result<()>>) -> Result<RecordingResult> {
    let mut p = match setup(&s) {
        Ok(p) => {
            let _ = init_tx.send(Ok(()));
            p
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = init_tx.send(Err(e));
            bail!(msg);
        }
    };
    drop(init_tx);

    let interval = Duration::from_nanos(1_000_000_000 / s.fps as u64);
    let tick100 = 10_000_000i64 / s.fps as i64;
    let mut t0: Option<Instant> = None;
    let mut next_k: u64 = 0;
    let mut have_new = false;
    let mut written = 0u64;
    let mut last_k: u64 = 0;

    let result = (|| -> Result<()> {
        loop {
            let timeout = match (t0, have_new) {
                (Some(t0), true) => {
                    (t0 + interval * next_k as u32).saturating_duration_since(Instant::now())
                }
                _ => Duration::from_millis(50),
            }
            .min(Duration::from_millis(50));
            match p.frame_rx.recv_timeout(timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            while let Ok(frame) = p.pool.TryGetNextFrame() {
                unsafe {
                    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                    let tex: ID3D11Texture2D = access.GetInterface()?;
                    p.ctx.CopySubresourceRegion(&p.staging, 0, 0, 0, 0, &tex, 0, Some(&p.src_box));
                }
                let _ = frame.Close();
                have_new = true;
                t0.get_or_insert_with(Instant::now);
            }

            if stop.load(Ordering::SeqCst) {
                break;
            }

            if let Some(t0) = t0 {
                let now_k = (t0.elapsed().as_nanos() / interval.as_nanos()) as u64;
                if have_new && now_k >= next_k {
                    write_staging(&mut p, now_k as i64 * tick100, tick100)?;
                    written += 1;
                    last_k = now_k;
                    next_k = now_k + 1;
                    have_new = false;
                }
            }
        }

        if let Some(t0) = t0 {
            // Re-emit the latest image at stop time so the video has the right length.
            let end_k = ((t0.elapsed().as_nanos() / interval.as_nanos()) as u64).max(last_k + 1);
            write_staging(&mut p, end_k as i64 * tick100, tick100)?;
            written += 1;
            last_k = end_k;
        }
        Ok(())
    })();

    let _ = p.session.Close();
    let _ = p.pool.Close();
    result?;
    if written == 0 {
        bail!("no frames were captured");
    }
    p.writer.finish()?;
    Ok(RecordingResult {
        path: s.path,
        width: s.region.w,
        height: s.region.h,
        duration_ms: (last_k + 1) * 1000 / s.fps as u64,
    })
}

fn write_staging(p: &mut Pipeline, time: i64, duration: i64) -> Result<()> {
    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        p.ctx.Map(&p.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let r = p.writer.write(mapped.pData as *const u8, mapped.RowPitch as usize, time, duration);
        p.ctx.Unmap(&p.staging, 0);
        r
    }
}
