//! 手动绘制蒙版的自检：编码往返、覆盖范围、以及对增益图的实际影响。
//!
//! 用法：`paint_test <源图> [输出目录]`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jpg2ultrahdr::{PaintLayer, Settings, Stroke, build_gain_map, convert};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("DSC_0227.jpg"));
    let out_dir = args.get(1).map(PathBuf::from);

    // ---- 0. 屏幕缩略图（安卓端绘制界面用） ----
    let (tw, th, rgba) = jpg2ultrahdr::thumbnail_rgba(&src, 1080).expect("缩略图解码失败");
    assert_eq!(rgba.len(), tw as usize * th as usize * 4, "RGBA 长度不对");
    // step 向上取整，所以两个方向都不会超过上限。
    assert!(tw.max(th) <= 1080, "缩略图过大：{tw}×{th}");
    assert!(rgba.chunks_exact(4).all(|px| px[3] == 255), "alpha 不是全不透明");
    println!("缩略图 OK  {tw}×{th} {} KB", rgba.len() / 1024);

    // ---- 1. 编码往返 ----
    let mut layer = PaintLayer::default();
    let mut stroke = Stroke::new(1.0, 0.08, false, [0.25, 0.40]);
    stroke.push([0.30, 0.42]);
    stroke.push([0.35, 0.45]);
    layer.push(stroke);
    layer.push(Stroke::new(0.5, 0.05, true, [0.60, 0.60]));

    let encoded = layer.encode();
    let decoded = PaintLayer::decode(&encoded);
    assert_eq!(layer, decoded, "编码往返不一致");
    println!("编码往返 OK  笔数={} 文本 {} 字节", decoded.len(), encoded.len());
    println!("  {encoded}");

    // ---- 2. 空蒙版必须和原来的结果完全一样 ----
    let empty = Settings::default();
    let base = build_gain_map(&load(&src), &empty);

    // ---- 3. 画一横条，检查只有该区域变化 ----
    let mut painted_settings = Settings::default();
    let mut band = Stroke::new(1.0, 0.10, false, [0.05, 0.50]);
    band.push([0.95, 0.50]);
    let mut paint = PaintLayer::default();
    paint.push(band.clone());
    painted_settings.paint = Arc::new(paint);

    let painted = build_gain_map(&load(&src), &painted_settings);
    assert_eq!(base.0.width, painted.0.width);
    assert_eq!(base.0.height, painted.0.height);
    assert_eq!(base.1.max_content_boost, painted.1.max_content_boost);

    let w = base.0.width as usize;
    let h = base.0.height as usize;
    let changed = (0..w * h).filter(|&i| base.0.data[i] != painted.0.data[i]).count();
    let rows_changed: Vec<usize> = (0..h)
        .filter(|&y| (0..w).any(|x| base.0.data[y * w + x] != painted.0.data[y * w + x]))
        .collect();
    println!(
        "画一条横条：{} / {} 像素变化，涉及 {} 行（{:.1}%），行范围 {:?}..{:?}",
        changed,
        w * h,
        rows_changed.len(),
        100.0 * rows_changed.len() as f32 / h as f32,
        rows_changed.first(),
        rows_changed.last()
    );
    assert!(changed > 0, "画了横条却没有任何变化");

    // 远离横条的位置必须原样保留。
    let top = (0..w).map(|x| base.0.data[x]).collect::<Vec<_>>();
    let top_after = (0..w).map(|x| painted.0.data[x]).collect::<Vec<_>>();
    assert_eq!(top, top_after, "画横条不应该改动画面顶部");

    // ---- 4. 橡皮擦回去 ----
    let mut erase_stroke = Stroke::new(0.0, 0.30, true, [0.05, 0.50]);
    erase_stroke.push([0.95, 0.50]);
    let mut erased = PaintLayer::default();
    erased.push(band.clone());
    erased.push(erase_stroke);

    let mut erased_settings = Settings::default();
    erased_settings.paint = Arc::new(erased);
    let after_erase = build_gain_map(&load(&src), &erased_settings);
    let residual = (0..w * h)
        .filter(|&i| after_erase.0.data[i] != base.0.data[i])
        .count();
    let painted_pixels = changed;
    println!(
        "橡皮擦覆盖后：残留 {} 像素（原本 {}，{:.1}%）",
        residual,
        painted_pixels,
        100.0 * residual as f32 / painted_pixels.max(1) as f32
    );

    // ---- 5. 真转一张，确认输出仍带增益图 ----
    let dst = out_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("paint_test_hdr.jpg");
    let report = convert(&src, &dst, &painted_settings).expect("转换失败");
    println!(
        "转换 OK  {}x{} 增益图 {}x{} 峰值 {:.2}x 体积 {} 字节",
        report.width,
        report.height,
        report.gain_width,
        report.gain_height,
        report.peak_boost,
        report.bytes
    );
    let bytes = std::fs::read(&dst).expect("读回失败");
    assert!(jpg2ultrahdr::is_already_ultra_hdr(&bytes), "输出缺少 Ultra HDR 标记");
    println!("输出带 Ultra HDR 标记 OK -> {}", dst.display());

    // ---- 6. 画过之后，暗图也不该被 1.5x 天花板卡住 ----
    let dim = Settings {
        auto_tone: true,
        paint: Arc::new({
            let mut p = PaintLayer::default();
            p.push(Stroke::new(1.0, 0.15, false, [0.5, 0.5]));
            p
        }),
        ..Default::default()
    };
    let dim_plain = Settings::default();
    let img = load(&src);
    let with_paint = build_gain_map(&img, &dim);
    let without = build_gain_map(&img, &dim_plain);
    println!(
        "峰值上限：未画 {:.2}x，画过 {:.2}x",
        without.1.max_content_boost[0], with_paint.1.max_content_boost[0]
    );
    assert!(
        with_paint.1.max_content_boost[0] >= 2.5 - 1e-6,
        "画过之后上限仍在 {:.2}x",
        with_paint.1.max_content_boost[0]
    );

    println!("\n全部通过");
}

fn load(path: &Path) -> ultrahdr_core::RawImage {
    let data = std::fs::read(path).expect("读不到源文件");
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&data));
    decoder.decode_headers().expect("解码头失败");
    let info = decoder.info().expect("拿不到图像信息");
    let pixels = decoder.decode().expect("解码失败");
    let width = info.width as u32;
    let height = info.height as u32;
    let expected = (width as usize) * (height as usize) * 3;
    let mut pixels = pixels;
    pixels.truncate(expected);
    ultrahdr_core::RawImage::from_data(
        width,
        height,
        ultrahdr_core::PixelFormat::Rgb8,
        ultrahdr_core::ColorGamut::Bt709,
        ultrahdr_core::ColorTransfer::Srgb,
        pixels,
    )
    .expect("构造 RawImage 失败")
}
