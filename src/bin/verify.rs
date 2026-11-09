//! 验证脚本：转换一张样张，并检查生成的 Ultra HDR 是否可被正确回读。
//! 用 `cargo run --bin verify -- <src.jpg> <dst.jpg> [max_boost]`。

use std::path::PathBuf;

use ultrahdr_core::gainmap::HdrOutputFormat;

use jpg2ultrahdr as convert;

fn main() {
    let mut args = std::env::args().skip(1);
    let src: PathBuf = args.next().expect("缺少源文件").into();
    let dst: PathBuf = args.next().expect("缺少目标文件").into();
    let max_boost: f32 = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);

    let settings = convert::Settings {
        max_boost,
        ..Default::default()
    };

    let report = convert::convert(&src, &dst, &settings).expect("转换失败");
    println!(
        "转换完成: {}x{} 增益图 {}x{} 峰值 {:.2}x 体积 {} 字节 用时 {} ms 色域 {}",
        report.width,
        report.height,
        report.gain_width,
        report.gain_height,
        report.peak_boost,
        report.bytes,
        report.elapsed_ms,
        report.gamut
    );

    // 回读：确认容器、增益图、元数据都在，并能重建 HDR。
    let bytes = std::fs::read(&dst).unwrap();
    let inspection = ultrajpeg::inspect(&bytes).unwrap();
    println!(
        "inspect: primary={} 字节 gain_map={:?} 字节 iso21496={} xmp={}",
        inspection.primary_jpeg_len,
        inspection.gain_map_jpeg_len,
        inspection.ultra_hdr.as_ref().and_then(|u| u.iso_21496_1.as_ref()).is_some(),
        inspection.ultra_hdr.as_ref().and_then(|u| u.xmp.as_ref()).is_some()
    );

    let decoded = ultrajpeg::decode(&bytes).unwrap();
    let gm = decoded.gain_map.as_ref().expect("缺少增益图");
    println!(
        "增益图像素: {}x{} 通道 {} 元数据 max_boost={:?} cap_max={}",
        gm.gain_map.width,
        gm.gain_map.height,
        gm.gain_map.channels,
        gm.metadata.as_ref().map(|m| m.max_content_boost),
        gm.metadata.as_ref().map(|m| m.hdr_capacity_max).unwrap_or(0.0)
    );

    // 增益图的统计：应当有明显的 0（未提亮区）和 255（峰值）。
    let data = &gm.gain_map.data;
    let zero = data.iter().filter(|&&b| b == 0).count();
    let max = data.iter().copied().max().unwrap_or(0);
    println!(
        "增益图分布: 0 值占比 {:.1}% 最大值 {} 非零均值 {:.1}",
        zero as f64 / data.len() as f64 * 100.0,
        max,
        data.iter().filter(|&&b| b > 0).map(|&b| b as f64).sum::<f64>()
            / (data.len() - zero).max(1) as f64
    );

    for boost in [1.0f32, 1.5, 2.0, max_boost] {
        let hdr = decoded
            .reconstruct_hdr(boost, HdrOutputFormat::LinearFloat)
            .expect("重建 HDR 失败");
        let mean = hdr.data.chunks_exact(4).map(|p| {
            f32::from_le_bytes([p[0], p[1], p[2], p[3]])
        }).sum::<f32>() / (hdr.width * hdr.height) as f32;
        println!("  display_boost={boost:.2} -> 线性均值 {mean:.4}");
    }

    // 与源图比较：SDR 主图应与原图基本一致（结构指标）。
    // 注意源文件同样不能用 ultrajpeg::decode（会把内嵌缩略图当成第二个码流）。
    let src_bytes = std::fs::read(&src).unwrap();
    let mut src_decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&src_bytes));
    let src_pixels = src_decoder.decode().expect("解码源图失败");
    let (src_w, src_h) = match src_decoder.info() {
        Some(i) => (i.width as u32, i.height as u32),
        None => panic!("无法读取源图尺寸"),
    };
    let out_primary = decoded.image;
    println!(
        "主图: {}x{} -> {}x{}  像素数一致={}",
        src_w,
        src_h,
        out_primary.width,
        out_primary.height,
        src_w == out_primary.width && src_h == out_primary.height
    );

    let mut diff = 0.0f64;
    let mut count = 0usize;
    if src_w == out_primary.width && src_h == out_primary.height {
        for (a, b) in src_pixels
            .chunks_exact(3)
            .zip(out_primary.data.chunks_exact(3))
        {
            for c in 0..3 {
                diff += (a[c] as f64 - b[c] as f64).abs();
                count += 1;
            }
        }
    }
    println!("SDR 主图平均绝对差: {:.2}/255", diff / count.max(1) as f64);
}
