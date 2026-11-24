//! Time each stage of a conversion, so we can tell where the milliseconds go.
//!
//! `convert()` is a single opaque call, and a GPU proposal is meaningless
//! without knowing which stage dominates. This bin re-runs the stages it can
//! reach from outside the crate and subtracts the total to get the encoder's
//! share.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use jpg2ultrahdr::{
    MaskStack, OutputFormat, Settings, auto_tone, build_gain_map_with, convert, output_path,
};
use ultrahdr_core::{ColorGamut, ColorTransfer, PixelFormat, RawImage};

fn decode(data: &[u8]) -> Result<RawImage> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(data));
    decoder
        .decode_headers()
        .context("读取 JPEG 头失败")?;
    let info = decoder.info().context("没有 JPEG 信息")?;
    let (width, height) = (info.width as u32, info.height as u32);
    let mut pixels = decoder.decode().context("解码 JPEG 失败")?;
    let expected = (width * height * 3) as usize;
    if pixels.len() < expected {
        anyhow::bail!("解码结果不完整：期望 {expected} 字节，实际 {}", pixels.len());
    }
    pixels.truncate(expected);
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

    let formats = [OutputFormat::Jpeg, OutputFormat::Heic];

    for format in formats {
        println!("=== {} ===", if format == OutputFormat::Jpeg { "JPG" } else { "HEIC" });

        let settings = Settings {
            format,
            ..Settings::default()
        };
        let out = output_path(&src, Some(&PathBuf::from(r"D:\Photos\Ajpg2hdr\_prof")), format);

        // warm the page cache so the read time is not a one-off disk hit
        let _ = std::fs::read(&src);

        let t = Instant::now();
        let data = std::fs::read(&src).context("读取失败")?;
        let read_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let primary = decode(&data)?;
        let decode_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let tone = auto_tone(&primary);
        let tone_ms = t.elapsed().as_secs_f64() * 1000.0;

        let masks = MaskStack::default();
        let t = Instant::now();
        let (gain, _metadata) = build_gain_map_with(&primary, settings.scale, tone, &masks);
        let gain_ms = t.elapsed().as_secs_f64() * 1000.0;
        let gain_pixels = (gain.width * gain.height) as u64;
        let source_pixels = (primary.width * primary.height) as u64;

        let t = Instant::now();
        let report = convert(&src, &out, &settings)?;
        let total_ms = t.elapsed().as_secs_f64() * 1000.0;

        // `convert` repeats read + decode + tone + gain map; whatever is left
        // over is the encoder plus the file write.
        let encode_ms = total_ms - read_ms - decode_ms - tone_ms - gain_ms;

        println!("图像 {}x{}  ({:.1} MP)", primary.width, primary.height, source_pixels as f64 / 1e6);
        println!("增益图 {}x{}  ({:.2} MP)", gain.width, gain.height, gain_pixels as f64 / 1e6);
        println!("读取     {read_ms:8.1} ms");
        println!("解码     {decode_ms:8.1} ms   ({:.1}% of total)", decode_ms / total_ms * 100.0);
        println!("自动曲线 {tone_ms:8.1} ms");
        println!("增益图   {gain_ms:8.1} ms   ({:.1}%)", gain_ms / total_ms * 100.0);
        println!("编码+写  {encode_ms:8.1} ms   ({:.1}%)", encode_ms / total_ms * 100.0);
        println!("合计     {total_ms:8.1} ms   体积 {} 字节", report.bytes);
        println!(
            "吞吐     {:.2} MP/s（按源图大小算）",
            source_pixels as f64 / 1e6 / (total_ms / 1000.0)
        );
        println!();
    }

    Ok(())
}
