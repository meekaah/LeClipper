//! Tray/window icons drawn at runtime, so there are no asset files to ship.

pub const SIZE: u32 = 32;

/// Idle: dark rounded square with a red dot. Recording: red dot with a white stop square.
pub fn rgba(recording: bool) -> Vec<u8> {
    let s = SIZE as f32;
    let mut px = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = ((y * SIZE + x) * 4) as usize;
            let d = ((fx - s / 2.0).powi(2) + (fy - s / 2.0).powi(2)).sqrt();
            let mut c = [0u8; 4];
            if !recording {
                // Rounded square background.
                let r = 7.0;
                let qx = (fx - s / 2.0).abs() - (s / 2.0 - r - 1.0);
                let qy = (fy - s / 2.0).abs() - (s / 2.0 - r - 1.0);
                let dist = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() - r;
                if dist < 0.5 {
                    let a = (0.5 - dist).clamp(0.0, 1.0);
                    c = [36, 36, 40, (a * 255.0) as u8];
                }
                if d < 8.5 {
                    let a = (8.5 - d).clamp(0.0, 1.0);
                    c = blend(c, [255, 59, 48], a);
                }
            } else {
                if d < 15.0 {
                    let a = (15.0 - d).clamp(0.0, 1.0);
                    c = [235, 45, 40, (a * 255.0) as u8];
                }
                if (fx - s / 2.0).abs() < 5.5 && (fy - s / 2.0).abs() < 5.5 {
                    c = [255, 255, 255, 255];
                }
            }
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    px
}

fn blend(under: [u8; 4], over: [u8; 3], a: f32) -> [u8; 4] {
    let mix = |u: u8, o: u8| (u as f32 * (1.0 - a) + o as f32 * a) as u8;
    let alpha = (under[3] as f32 / 255.0 * (1.0 - a) + a) * 255.0;
    [mix(under[0], over[0]), mix(under[1], over[1]), mix(under[2], over[2]), alpha as u8]
}
