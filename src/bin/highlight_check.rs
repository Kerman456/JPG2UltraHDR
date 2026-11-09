//! 量化检查：在指定显示亮度下，高光区的实际提亮倍数是否达到设定值。

use ultrahdr_core::HdrOutputFormat;
use ultrahdr_core::color::{luma_coefficients, srgb_eotf};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("用法: highlight_check <file_hdr.jpg> [max_boost]");
    let max_boost: f32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);

    let data = std::fs::read(&path).expect("读取失败");
    let decoded = ultrajpeg::decode(&data).expect("解码失败");
    let sdr = &decoded.image;
    let luma = luma_coefficients(sdr.gamut);

    let hdr = decoded
        .reconstruct_hdr(max_boost, HdrOutputFormat::LinearFloat)
        .expect("重建失败");

    let pixels = (sdr.width as usize) * (sdr.height as usize);
    let mut ratios: Vec<(f32, f32)> = Vec::with_capacity(pixels);
    let mut clipped = 0usize;

    for i in 0..pixels {
        let s = &sdr.data[i * 3..i * 3 + 3];
        let sdr_lum = luma[0] * srgb_eotf(s[0] as f32 / 255.0)
            + luma[1] * srgb_eotf(s[1] as f32 / 255.0)
            + luma[2] * srgb_eotf(s[2] as f32 / 255.0);

        let h = &hdr.data[i * 16..i * 16 + 16];
        let r = f32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let g = f32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let b = f32::from_le_bytes([h[8], h[9], h[10], h[11]]);
        let hdr_lum = luma[0] * r + luma[1] * g + luma[2] * b;

        if hdr_lum > 1.0 {
            clipped += 1;
        }
        ratios.push((sdr_lum, hdr_lum));
    }

    ratios.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    println!("像素数 {pixels}, 平均 SDR 亮度 {:.4}", ratios.iter().map(|r| r.0 as f64).sum::<f64>() / pixels as f64);

    // 最亮的几个百分位区间，看实际倍数。
    for pct in [50.0f64, 90.0, 99.0, 99.9] {
        let idx = ((pixels as f64) * pct as f64 / 100.0) as usize;
        let idx = idx.min(pixels - 1);
        let (s, h) = ratios[idx];
        let ratio = if s > 1e-5 { h / s } else { f32::NAN };
        println!("  {pct}% 分位: SDR {s:.4} -> HDR {h:.4}, 实际倍数 {ratio:.2}x");
    }

    // 最亮 1% 的平均倍数
    let start = pixels * 99 / 100;
    let top = &ratios[start..];
    let mean_s: f64 = top.iter().map(|r| r.0 as f64).sum::<f64>() / top.len() as f64;
    let mean_h: f64 = top.iter().map(|r| r.1 as f64).sum::<f64>() / top.len() as f64;
    println!(
        "最亮 1%: SDR {mean_s:.4} -> HDR {mean_h:.4}, 平均倍数 {:.2}x (设定 {max_boost:.2}x)",
        mean_h / mean_s.max(1e-9)
    );
    println!(
        "HDR 超过 1.0 线性亮度的像素: {clipped} ({:.2}%)",
        clipped as f64 * 100.0 / pixels as f64
    );
    println!(
        "最大 HDR 亮度: {:.4}",
        ratios.last().map(|r| r.1).unwrap_or(0.0)
    );
}
