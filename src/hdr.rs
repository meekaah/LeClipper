//! HDR monitor support: detect whether a monitor is in HDR (advanced color) mode, and
//! read its "SDR content brightness" so captured scRGB can be mapped back to what an
//! SDR viewer expects.

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

/// For an HDR monitor, the scRGB value of SDR white (1.0 = 80 nits). `None` for SDR monitors.
pub fn sdr_white_scale(monitor: HMONITOR) -> Option<f32> {
    let device_name = hdr_output_name(monitor)?;
    let level = sdr_white_level(&device_name).unwrap_or_else(|| {
        crate::log("HDR: could not read SDR white level, assuming 200 nits");
        2500 // 200 nits; SDRWhiteLevel is in units of 80 nits / 1000
    });
    Some(level as f32 / 1000.0)
}

/// GDI device name (e.g. `\\.\DISPLAY1`) of the monitor if it is currently in HDR mode.
fn hdr_output_name(monitor: HMONITOR) -> Option<[u16; 32]> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let Ok(out6) = output.cast::<IDXGIOutput6>() else { continue };
                let Ok(desc) = out6.GetDesc1() else { continue };
                if desc.Monitor == monitor {
                    return (desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                        .then_some(desc.DeviceName);
                }
            }
        }
        None
    }
}

fn sdr_white_level(device_name: &[u16; 32]) -> Option<u32> {
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let name_len = device_name.iter().position(|&c| c == 0).unwrap_or(32);
        for path in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let src_len = src.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(32);
            if src.viewGdiDeviceName[..src_len] != device_name[..name_len] {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                SDRWhiteLevel: 0,
            };
            if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                return Some(white.SDRWhiteLevel);
            }
        }
        None
    }
}
