//! Does `heifer` scale badly in a batch because each image grabs every core?
//!
//! `EncodeOptions::max_threads == 0` means "use all cores". With eight images
//! in flight that is eight times over-subscription, and the batch throughput
//! barely moves. This measures per-image cost at several thread budgets so the
//! tradeoff between "one image fast" and "a batch fast" is a number, not a
//! guess.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use jpg2ultrahdr::{auto_tone, build_gain_map_with, Settings};
use ultrahdr_core::color::{luma_coefficients, srgb_eotf};
use ultrahdr_core::{ColorGamut, ColorTransfer, PixelFormat, RawImage};

fn decode(data: &[u8]) -> Result<RawImage> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(data));
    decoder.decode_headers().context("读取 JPEG 头失败")?;
    let info = decoder.info().context("没有 JPEG 信息")?;
    let (width, height) = (info.width as u32, info.height as u32);
    let mut pixels = decoder.decode().context("解码 JPEG 失败")?;
    pixels.truncate((width * height * 3) as usize);
    RawImage::from_data(
        width,
        height,
        PixelFormat::Rgb8,
        ColorGamut::Bt709,
        ColorTransfer::Srgb,
        pixels,
    )
    .context("构造图像失败")
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\Photos\0425\照片\未命名导出\DSC_0227.jpg"));

    let data = std::fs::read(&src).context("读取失败")?;
    let primary = decode(&data)?;
    let settings = Settings::default();
    let tone = auto_tone(&primary);
    let (gain, metadata) = build_gain_map_with(&primary, settings.scale, tone, &Default::default());
    let max_boost = metadata.max_content_boost[0];
    let log_max = max_boost.ln();
    let luma = luma_coefficients(primary.gamut);

    // Rebuild the base/HDR pair exactly as `heic::encode` does.
    let mut base = heifer::Image::<u16> {
        width: primary.width,
        height: primary.height,
        bit_depth: 8,
        has_alpha: false,
        data: Vec::with_capacity((primary.width * primary.height * 4) as usize),
    };
    let mut hdr = heifer::HdrImage {
        width: primary.width,
        height: primary.height,
        headroom: max_boost,
        data: Vec::with_capacity((primary.width * primary.height * 3) as usize),
    };
    let scale_x = primary.width.div_ceil(gain.width.max(1)).max(1);
    let scale_y = primary.height.div_ceil(gain.height.max(1)).max(1);
    for y in 0..primary.height {
        let gy = ((y / scale_y).min(gain.height - 1) * gain.width) as usize;
        for x in 0..primary.width {
            let i = ((y * primary.width + x) * 3) as usize;
            let (r, g, b) = (
                primary.data[i] as f32,
                primary.data[i + 1] as f32,
                primary.data[i + 2] as f32,
            );
            let lum = luma[0] * r + luma[1] * g + luma[2] * b;
            let _ = srgb_eotf((lum / 255.0).clamp(0.0, 1.0));
            let gx = ((x / scale_x).min(gain.width - 1)) as usize;
            let factor = ((gain.data[gy + gx] as f32 / 255.0) * log_max).exp();
            base.data
                .extend_from_slice(&[r as u16, g as u16, b as u16, u16::MAX]);
            hdr.data.push(srgb_eotf(r / 255.0) * factor);
            hdr.data.push(srgb_eotf(g / 255.0) * factor);
            hdr.data.push(srgb_eotf(b / 255.0) * factor);
        }
    }

    println!("图像 {}x{}  上限 {max_boost:.2}×", primary.width, primary.height);

    for threads in [0usize, 1, 2, 4] {
        let options = heifer::EncodeOptions::new()
            .with_quality(80)
            .with_max_threads(threads);
        let t = Instant::now();
        let out = heifer::encode_hdr(&base, &hdr, &options).context("HEIC 编码失败")?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        // Projected batch throughput if `copies` images run concurrently and
        // each is allowed `threads` cores (0 = all 24).
        let cores_used = if threads == 0 { 24 } else { threads };
        let copies = (24 / cores_used.max(1)).max(1);
        let batch_ms = ms * copies as f64 / copies as f64; // per wave
        println!(
            "max_threads={threads:<2} 单张 {ms:6.0} ms  {} 字节   可用并发≈{copies}   一浪 {batch_ms:6.0} ms ⇒ {:.2} 张/秒",
            out.len(),
            copies as f64 / (ms / 1000.0)
        );
    }

    Ok(())
}
