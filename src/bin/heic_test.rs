//! Check that the HEIC writer really preserves the gain map we computed.
//!
//! The JPEG path can be inspected structurally (`inspect_hdr`), but HEIC stores
//! the gain map as a HEVC-coded plane derived from a base/HDR pair. The only
//! honest check is to decode the file again and compare the gains it implies
//! against the ones `build_gain_map_with` produced.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use jpg2ultrahdr::{
    MaskStack, OutputFormat, Settings, auto_tone, build_gain_map_with, convert,
    is_already_ultra_hdr, output_path,
};
use ultrahdr_core::color::{luma_coefficients, srgb_eotf};
use ultrahdr_core::{ColorGamut, ColorTransfer, PixelFormat, RawImage};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\Photos\0425\照片\未命名导出\DSC_0227.jpg"));

    let data = std::fs::read(&src).with_context(|| format!("读取 {} 失败", src.display()))?;
    if is_already_ultra_hdr(&data) {
        bail!("源文件已经是 Ultra HDR 图像");
    }

    // ---- what the writer is told to embed -------------------------------
    let primary = decode(&data)?;
    let settings = Settings {
        format: OutputFormat::Heic,
        ..Settings::default()
    };
    let tone = auto_tone(&primary);
    let masks = MaskStack::default();
    let (gain, metadata) = build_gain_map_with(&primary, settings.scale, tone, &masks);
    let max_boost = metadata.max_content_boost[0];
    println!(
        "曲线: 起点 {:.3}  上限 {max_boost:.3}×  顶部 {:.2} 档",
        tone.knee, tone.top_stops
    );
    println!(
        "增益图 {}x{} Gray8  0 值 {:.1}%  非零均值 {:.1}",
        gain.width,
        gain.height,
        zero_ratio(&gain) * 100.0,
        nonzero_mean(&gain)
    );

    // ---- encode ---------------------------------------------------------
    let out_dir = Path::new(r"D:\Photos\Ajpg2hdr\_heic");
    let dst = output_path(&src, Some(out_dir), OutputFormat::Heic);
    let report = convert(&src, &dst, &settings).with_context(|| "转换失败")?;
    println!(
        "转换 OK  {}x{}  体积 {} 字节  用时 {} ms  写入 {}",
        report.width,
        report.height,
        report.bytes,
        report.elapsed_ms,
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("?")
    );

    // ---- decode and compare --------------------------------------------
    let heic = std::fs::read(&dst).with_context(|| "读回 HEIC 失败")?;
    if heic.len() < 12 || &heic[4..8] != b"ftyp" {
        bail!("输出不像 ISOBMFF 容器");
    }
    let brand = &heic[8..12];
    println!("ftyp brand = {}", String::from_utf8_lossy(brand));

    // The EXIF the source carried must still be in the container: without it
    // the orientation of a camera JPEG is lost and the photo shows up sideways.
    let exif_markers = count_subslice(&heic, b"Exif\0\0");
    let icc_markers = count_subslice(&heic, b"prof");
    println!("容器内 Exif 标记 {exif_markers} 处，ICC 'prof' 标记 {icc_markers} 处");
    if exif_markers == 0 {
        println!("注意：容器里找不到 Exif 头（源文件可能本来就没有）");
    }

    let back = heifer::hdr::decode_hdr_with_options(
        &heic,
        &heifer::Options::new(),
        f32::INFINITY,
    )
    .map_err(|e| anyhow::anyhow!("解码 HEIC 失败：{e:?}"))?;
    println!(
        "回读 {}x{}  headroom {:.4}",
        back.width, back.height, back.headroom
    );

    // A mask-limited gain map must survive too. The layer lowers the knee by
    // 0.15, so the painted band has to decode visibly brighter than the same
    // pixels under the plain global curve — otherwise the mask was dropped and
    // the "inside > SDR white" count alone would still pass, because the global
    // curve already lifts some highlights inside the band.
    let mut masked = MaskStack::default();
    {
        use jpg2ultrahdr::{BrushMask, BrushStroke, MaskCombine, MaskLayer, MaskShape};
        let mut stroke = BrushStroke::new(0.16, 1.0, 1.0, false, [0.30, 0.30]);
        stroke.push([0.70, 0.70]);
        masked.push(MaskLayer {
            name: "HEIC 测试".into(),
            enabled: true,
            invert: false,
            opacity: 1.0,
            combine: MaskCombine::Add,
            knee: (tone.knee - 0.15).clamp(0.0, 0.9),
            max_boost,
            follow_base: false,
            shape: MaskShape::Brush(BrushMask { strokes: vec![stroke] }),
        });
    }
    let (masked_gain, masked_meta) =
        build_gain_map_with(&primary, settings.scale, tone, &masked);
    let masked_heic = jpg2ultrahdr::heic::encode(
        &primary,
        &masked_gain,
        &masked_meta,
        settings.primary_quality,
        None,
        None,
    )?;
    let masked_back =
        heifer::hdr::decode_hdr_with_options(&masked_heic, &heifer::Options::new(), f32::INFINITY)
            .map_err(|e| anyhow::anyhow!("解码带蒙版 HEIC 失败：{e:?}"))?;

    // Compare like with like: same pixels, only the gain map differs between the
    // two decodes, so any difference is the mask's doing.
    let mut inside = 0u64;
    let mut inside_max_masked = 0.0f32;
    let mut inside_max_plain = 0.0f32;
    let mut inside_sum_masked = 0.0f64;
    let mut inside_sum_plain = 0.0f64;
    let mut inside_count = 0u64;
    for y in (0..primary.height as usize).step_by(5) {
        for x in (0..primary.width as usize).step_by(5) {
            let (u, v) = (x as f32 / primary.width as f32, y as f32 / primary.height as f32);
            if !((0.30..=0.70).contains(&u) && (0.30..=0.70).contains(&v)) {
                continue;
            }
            let q = (y * masked_back.width as usize + x) * 3;
            let value = masked_back.data[q]
                .max(masked_back.data[q + 1])
                .max(masked_back.data[q + 2]);
            let plain = back.data[q].max(back.data[q + 1]).max(back.data[q + 2]);
            inside_max_masked = inside_max_masked.max(value);
            inside_max_plain = inside_max_plain.max(plain);
            inside_sum_masked += f64::from(value);
            inside_sum_plain += f64::from(plain);
            inside_count += 1;
            if value > 1.0 {
                inside += 1;
            }
        }
    }
    let n = inside_count.max(1) as f64;
    println!(
        "画过区域：带蒙版峰值 {:.3}× 均值 {:.4}× / 无蒙版峰值 {:.3}× 均值 {:.4}×（>SDR 白 {inside} 点，共 {inside_count}）",
        inside_max_masked,
        inside_sum_masked / n,
        inside_max_plain,
        inside_sum_plain / n
    );

    // Only meaningful on a picture with highlight energy left to expand. A pure
    // black frame has nothing to lift, and a pure white one already sits at the
    // ceiling, so the mask cannot make either brighter — that is correct
    // behaviour, not a dropped mask.
    let empty_gain = gain.data.iter().all(|&b| b == 0);
    let saturated = inside_max_plain >= max_boost * 0.999;
    if empty_gain || saturated {
        println!(
            "（跳过蒙版比对：{}）",
            if empty_gain { "增益图为空" } else { "画过区域已在上限" }
        );
    } else {
        if inside == 0 {
            bail!("蒙版区域没有出现任何超过 SDR 白的像素，蒙版没有进入 HEIC");
        }
        if inside_max_masked <= inside_max_plain {
            bail!(
                "画过区域没有比无蒙版更亮（{inside_max_masked:.3} <= {inside_max_plain:.3}），蒙版被忽略"
            );
        }
    }

    let luma = luma_coefficients(ColorGamut::Bt709);
    let gw = gain.width as usize;
    let gs = gain.stride as usize;
    let scale_x = primary.width.div_ceil(gain.width).max(1) as usize;
    let scale_y = primary.height.div_ceil(gain.height).max(1) as usize;

    // Relative error is meaningless near black: both sides are a few thousandths
    // of SDR white, so a one-code-value difference reads as 100%. Restrict the
    // comparison to pixels that carry visible highlight energy and report the
    // percentiles, which is what "did the gain map survive" actually means.
    let floor = 0.02f32; // 2% of SDR white
    let mut rel: Vec<f32> = Vec::new();
    let mut over = 0u64;
    let mut considered = 0u64;
    for y in (0..primary.height as usize).step_by(3) {
        for x in (0..primary.width as usize).step_by(3) {
            let gx = (x / scale_x).min(gw - 1);
            let gy = (y / scale_y).min(gain.height as usize - 1);
            let byte = gain.data[gy * gs + gx];
            let factor = ((byte as f32 / 255.0) * max_boost.ln()).exp();

            let p = y * primary.stride as usize + x * 3;
            let base = [
                srgb_eotf(primary.data[p] as f32 / 255.0),
                srgb_eotf(primary.data[p + 1] as f32 / 255.0),
                srgb_eotf(primary.data[p + 2] as f32 / 255.0),
            ];
            let expect = (luma[0] * base[0] + luma[1] * base[1] + luma[2] * base[2]) * factor;

            let q = (y * back.width as usize + x) * 3;
            let got =
                luma[0] * back.data[q] + luma[1] * back.data[q + 1] + luma[2] * back.data[q + 2];

            if got > 1.0 {
                over += 1;
            }
            if expect < floor {
                continue;
            }
            rel.push(((got - expect) / expect).abs());
            considered += 1;
        }
    }
    if rel.is_empty() {
        // A pure black frame has no pixel above the floor; there is nothing to
        // compare, which is not a failure.
        println!("（跳过保真误差：没有亮度 ≥ {floor} 的像素）");
        return Ok(());
    }
    rel.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f32| rel[((rel.len() - 1) as f32 * q) as usize] * 100.0;
    let mean = rel.iter().map(|&v| f64::from(v)).sum::<f64>() / rel.len() as f64;
    println!(
        "重建误差（亮度 ≥ {floor}）：平均 {:.2}%  P50 {:.2}%  P99 {:.1}%  P99.9 {:.1}%",
        mean * 100.0,
        at(0.50),
        at(0.99),
        at(0.999)
    );
    println!(
        "HDR>1.0 占 {:.2}%（{} 像素，{} 参与比较）",
        over as f64 / (back.width as f64 * back.height as f64) * 100.0,
        over,
        considered
    );

    if mean > 0.06 {
        bail!("平均误差超过 6%，增益图没有被正确保留");
    }
    if at(0.99) > 25.0 {
        bail!("P99 误差 {:.1}% 过大", at(0.99));
    }
    if back.headroom < 1.5 {
        bail!("回读 headroom 只有 {:.3}，HDR 信息丢了", back.headroom);
    }
    if back.width != primary.width || back.height != primary.height {
        bail!(
            "尺寸不一致：写入 {}x{}，回读 {}x{}",
            primary.width,
            primary.height,
            back.width,
            back.height
        );
    }
    if over == 0 && !empty_gain {
        bail!("回读结果里没有任何像素超过 SDR 白，增益图没有生效");
    }
    println!("HEIC 增益图保真 OK");
    Ok(())
}

fn count_subslice(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn zero_ratio(gain: &RawImage) -> f32 {    let zeros = gain.data.iter().filter(|&&b| b == 0).count();
    zeros as f32 / gain.data.len().max(1) as f32
}

fn nonzero_mean(gain: &RawImage) -> f32 {
    let mut sum = 0u64;
    let mut count = 0u64;
    for &b in &gain.data {
        if b != 0 {
            sum += u64::from(b);
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        sum as f32 / count as f32
    }
}

fn decode(data: &[u8]) -> Result<RawImage> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(data));
    decoder
        .decode_headers()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (width, height) = match decoder.info() {
        Some(info) => (info.width as u32, info.height as u32),
        None => bail!("无法读取图像尺寸"),
    };
    let mut pixels = decoder.decode().map_err(|e| anyhow::anyhow!("{e}"))?;
    pixels.truncate((width as usize) * (height as usize) * 3);
    RawImage::from_data(
        width,
        height,
        PixelFormat::Rgb8,
        ColorGamut::Bt709,
        ColorTransfer::Srgb,
        pixels,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
