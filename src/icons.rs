//! Menu-bar icons, rasterised at startup so the binary ships no image files.

use tray_icon::Icon;

const S: usize = 44; // 22pt @2x

fn sd_round_rect(px: f32, py: f32, half: f32, r: f32) -> f32 {
    let qx = px.abs() - half + r;
    let qy = py.abs() - half + r;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn raster(f: impl Fn(f32, f32) -> [f32; 4]) -> Icon {
    let mut buf = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            // Centre-relative coordinates, supersampled 4x for smooth edges.
            let mut acc = [0f32; 4];
            for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let c = f(x as f32 + sx - S as f32 / 2.0, y as f32 + sy - S as f32 / 2.0);
                for i in 0..4 {
                    acc[i] += c[i] * c[3] / 4.0;
                }
            }
            let a = acc[3];
            let i = (y * S + x) * 4;
            if a > 0.0 {
                buf[i] = (acc[0] / a * 255.0) as u8;
                buf[i + 1] = (acc[1] / a * 255.0) as u8;
                buf[i + 2] = (acc[2] / a * 255.0) as u8;
                buf[i + 3] = (a * 255.0) as u8;
            }
        }
    }
    Icon::from_rgba(buf, S as u32, S as u32).unwrap()
}

/// Selection corners around a dot — rendered as a template (system tinted).
pub fn idle() -> Icon {
    raster(|x, y| {
        let d = sd_round_rect(x, y, 15.0, 6.0).abs() - 1.6;
        let corner = x.abs() > 6.5 && y.abs() > 6.5;
        let ring = d <= 0.0 && corner;
        let dot = (x * x + y * y).sqrt() <= 4.0;
        if ring || dot { [0.0, 0.0, 0.0, 1.0] } else { [0.0; 4] }
    })
}

/// Red record button with a stop square — shown while recording.
pub fn recording() -> Icon {
    raster(|x, y| {
        let disc = (x * x + y * y).sqrt() <= 15.0;
        let stop = sd_round_rect(x, y, 5.5, 1.5) <= 0.0;
        if stop {
            [1.0, 1.0, 1.0, 1.0]
        } else if disc {
            [1.0, 0.23, 0.19, 1.0]
        } else {
            [0.0; 4]
        }
    })
}
