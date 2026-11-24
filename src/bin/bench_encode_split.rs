//! Split the JPG encode cost between the primary image and the gain map.
//!
//! Both go through the same entropy coder, so the only honest way to attribute
//! the time is to encode the same primary twice: once alone and once with a
//! gain map attached. The difference is the gain map's share.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use jpg2ultrahdr::{auto_tone, build_gain_map_with, Settings};
use ultrahdr_core::{ColorGamut, ColorTransfer, PixelFormat, RawImage};
use ultrajpeg::{ChromaSubsampling, ColorMetadata, CompressionEffort, EncodeOptions, GainMapBundle, PrimaryMetadata};

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

    println!(
        "主图 {}x{}  ({:.2} MP)   增益图 {}x{}  ({:.2} MP)",
        primary.width,
        primary.height,
        (primary.width as f64 * primary.height as f64) / 1e6,
        gain.width,
        gain.height,
        (gain.width as f64 * gain.height as f64) / 1e6
    );

    let base = |gain_map: Option<GainMapBundle>, progressive: bool| EncodeOptions {
        quality: settings.primary_quality.clamp(1, 100),
        progressive,
        compression: CompressionEffort::Balanced,
        chroma_subsampling: ChromaSubsampling::Yuv420,
        primary_metadata: PrimaryMetadata {
            color: ColorMetadata::default(),
            exif: None,
        },
        ultra_hdr_metadata_emission: Default::default(),
        gain_map,
    };

    let bundle = || GainMapBundle {
        image: gain.clone(),
        metadata: metadata.clone(),
        quality: settings.gainmap_quality.clamp(1, 100),
        progressive: false,
        compression: CompressionEffort::Balanced,
    };

    // best of three, to take scheduler noise out of it
    let best = |options: EncodeOptions, label: &str| -> Result<f64> {
        let mut best_ms = f64::MAX;
        for _ in 0..3 {
            let opts = EncodeOptions {
                gain_map: options.gain_map.clone(),
                ..options.clone()
            };
            let t = Instant::now();
            let out = ultrajpeg::encode(&primary, &opts).context("编码失败")?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms < best_ms {
                best_ms = ms;
            }
            std::hint::black_box(&out);
        }
        println!("{label:<34} {best_ms:7.0} ms");
        Ok(best_ms)
    };

    let primary_only = best(base(None, false), "主图 only（基线 JPEG）")?;
    let primary_prog = best(base(None, true), "主图 only（渐进式 JPEG）")?;
    let with_gain = best(base(Some(bundle()), false), "主图 + 增益图（当前实际路径）")?;
    let with_gain_prog = best(base(Some(bundle()), true), "主图 + 增益图（渐进式）")?;

    println!();
    println!("增益图增量（基线）    {:7.0} ms  ({:.0}% of {:.0})", with_gain - primary_only, (with_gain - primary_only) / with_gain * 100.0, with_gain);
    println!("渐进式增量            {:7.0} ms  ({:.0}% of {:.0})", primary_prog - primary_only, (primary_prog - primary_only) / primary_prog * 100.0, primary_prog);
    println!("当前路径总编码        {with_gain_prog:7.0} ms");
    println!("其中主图占            {:.0}%", (with_gain_prog - (with_gain - primary_only)) / with_gain_prog * 100.0);

    Ok(())
}
