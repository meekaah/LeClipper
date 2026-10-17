//! Region recording: Windows.Graphics.Capture on one monitor, GPU crop into a staging
//! texture, paced to the target fps and fed to the MP4 writer.
//!
//! On an HDR monitor the frames are captured as scRGB (FP16) and converted to sRGB on the
//! GPU using the monitor's SDR white level. Capturing HDR as BGRA8 directly makes Windows
//! do its own conversion, which ignores the "SDR content brightness" setting and comes
//! out much brighter than what is on screen.

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
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_DRIVER_TYPE_HARDWARE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
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

/// GPU pass that turns an FP16 scRGB crop into 8-bit sRGB (stored in BGRA byte order).
struct ToneMap {
    /// FP16 crop of the captured frame; frames are copied here instead of into `staging`.
    crop: ID3D11Texture2D,
    out: ID3D11Texture2D,
    srv: Option<ID3D11ShaderResourceView>,
    uav: Option<ID3D11UnorderedAccessView>,
    cbuf: Option<ID3D11Buffer>,
    shader: ID3D11ComputeShader,
    groups: (u32, u32),
}

const TONEMAP_HLSL: &str = r#"
Texture2D<float4> src : register(t0);
RWTexture2D<unorm float4> dst : register(u0);
cbuffer C : register(b0) { float inv_white; float3 pad; };

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint w, h;
    dst.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;
    // scRGB is linear BT.709 with 1.0 = 80 nits; SDR white maps to 1.0, brighter clips.
    float3 c = saturate(src[id.xy].rgb * inv_white);
    float3 s = c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    // The texture is RGBA8 but the encoder reads BGRA bytes, so store swizzled.
    dst[id.xy] = float4(s.bgr, 1.0);
}
"#;

struct Pipeline {
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    tonemap: Option<ToneMap>,
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

        let tonemap = match crate::hdr::sdr_white_scale(HMONITOR(s.region.monitor as _)) {
            Some(white) => {
                crate::log(&format!("HDR monitor, SDR white = {:.0} nits", white * 80.0));
                Some(create_tonemap(&device, r.w, r.h, white)?)
            }
            None => None,
        };
        // RGBA8 holding BGRA bytes when tone mapping (UAVs can't be BGRA), else plain BGRA8.
        let staging_format =
            if tonemap.is_some() { DXGI_FORMAT_R8G8B8A8_UNORM } else { DXGI_FORMAT_B8G8R8A8_UNORM };
        let staging = create_texture(
            &device, r.w, r.h, staging_format, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32,
        )?;

        let writer = Mp4Writer::create(&s.path, r.w, r.h, s.fps, s.bitrate)?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            if tonemap.is_some() {
                DirectXPixelFormat::R16G16B16A16Float
            } else {
                DirectXPixelFormat::B8G8R8A8UIntNormalized
            },
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

        Ok(Pipeline { ctx, staging, tonemap, pool, session, writer, frame_rx, src_box })
    }
}

fn create_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.context("CreateTexture2D")
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

fn create_tonemap(device: &ID3D11Device, w: u32, h: u32, white: f32) -> Result<ToneMap> {
    unsafe {
        let crop = create_texture(
            device, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32, 0,
        )?;
        let out = create_texture(
            device, w, h, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_USAGE_DEFAULT,
            D3D11_BIND_UNORDERED_ACCESS.0 as u32, 0,
        )?;
        let mut srv = None;
        device.CreateShaderResourceView(&crop, None, Some(&mut srv))?;
        let mut uav = None;
        device.CreateUnorderedAccessView(&out, None, Some(&mut uav))?;

        let consts = [1.0 / white, 0.0, 0.0, 0.0f32];
        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of_val(&consts) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: consts.as_ptr().cast(), ..Default::default() };
        let mut cbuf = None;
        device.CreateBuffer(&cb_desc, Some(&init), Some(&mut cbuf))?;

        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        if let Err(e) = D3DCompile(
            TONEMAP_HLSL.as_ptr().cast(),
            TONEMAP_HLSL.len(),
            windows::core::s!("tonemap"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        ) {
            let msg = errors.as_ref().map(|b| String::from_utf8_lossy(blob_bytes(b)).into_owned());
            bail!("compiling HDR tone-map shader: {e} {}", msg.unwrap_or_default());
        }
        let code = code.context("no shader bytecode")?;
        let mut shader = None;
        device.CreateComputeShader(blob_bytes(&code), None, Some(&mut shader))?;

        Ok(ToneMap {
            crop,
            out,
            srv,
            uav,
            cbuf,
            shader: shader.context("CreateComputeShader")?,
            groups: (w.div_ceil(8), h.div_ceil(8)),
        })
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
                    let dst = p.tonemap.as_ref().map_or(&p.staging, |t| &t.crop);
                    p.ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, &tex, 0, Some(&p.src_box));
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
        if let Some(t) = &p.tonemap {
            p.ctx.CSSetShader(&t.shader, None);
            p.ctx.CSSetShaderResources(0, Some(&[t.srv.clone()]));
            p.ctx.CSSetUnorderedAccessViews(0, 1, Some(&t.uav), None);
            p.ctx.CSSetConstantBuffers(0, Some(&[t.cbuf.clone()]));
            p.ctx.Dispatch(t.groups.0, t.groups.1, 1);
            // Unbind so `crop` can be a copy destination again.
            p.ctx.CSSetShaderResources(0, Some(&[None]));
            p.ctx.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
            p.ctx.CopyResource(&p.staging, &t.out);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        p.ctx.Map(&p.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let r = p.writer.write(mapped.pData as *const u8, mapped.RowPitch as usize, time, duration);
        p.ctx.Unmap(&p.staging, 0);
        r
    }
}
