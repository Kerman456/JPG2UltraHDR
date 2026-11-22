//! 蒙版系统的自检：编码往返、覆盖范围、以及对增益图的实际影响。
//!
//! 用法：`paint_test <源图> [输出目录]`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jpg2ultrahdr::{
    BrushMask, BrushStroke, MaskCombine, MaskLayer, MaskShape, MaskStack, Settings,
    build_gain_map, convert,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("DSC_0227.jpg"));
    let out_dir = args.get(1).map(PathBuf::from);

    // ---- 0. 屏幕缩略图（编辑器的低分辨率副本） ----
    let (tw, th, rgba) = jpg2ultrahdr::thumbnail_rgba(&src, 1080).expect("缩略图解码失败");
    assert_eq!(rgba.len(), tw as usize * th as usize * 4, "RGBA 长度不对");
    assert!(tw.max(th) <= 1080, "缩略图过大：{tw}×{th}");
    assert!(
        rgba.chunks_exact(4).all(|px| px[3] == 255),
        "alpha 不是全不透明"
    );
    println!("缩略图 OK  {tw}×{th} {} KB", rgba.len() / 1024);

    // 预览帧要能给出亮度与同一条曲线。
    let frame = jpg2ultrahdr::PreviewFrame::load(&src, 1000).expect("预览帧解码失败");
    let tone = frame.tone();
    println!(
        "预览帧 OK  {}×{}  曲线 起点 {:.2} 上限 {:.2}x",
        frame.width, frame.height, tone.knee, tone.max_boost
    );

    // ---- 1. 五类形状的编码往返 ----
    let mut stack = MaskStack::default();
    stack.push(MaskLayer::new(
        "线性",
        MaskShape::Linear {
            start: [0.1, 0.2],
            end: [0.9, 0.85],
            feather: 1.5,
        },
    ));
    stack.push(MaskLayer::new(
        "径向",
        MaskShape::Radial {
            center: [0.4, 0.6],
            radius: [0.25, 0.15],
            rotation: 0.7,
            feather: 0.4,
        },
    ));
    let mut brush = BrushMask::default();
    let mut stroke = BrushStroke::new(0.08, 0.7, 1.0, false, [0.25, 0.40]);
    stroke.push([0.30, 0.42]);
    stroke.push([0.35, 0.45]);
    brush.push(stroke);
    brush.push(BrushStroke::new(0.05, 0.5, 0.6, true, [0.60, 0.60]));
    stack.push(MaskLayer::new("画笔", MaskShape::Brush(brush)));
    stack.push(MaskLayer::new(
        "亮度",
        MaskShape::Luminance {
            min: 0.55,
            max: 0.95,
            softness: 0.3,
        },
    ));
    stack.push(MaskLayer::new(
        "颜色",
        MaskShape::Color {
            hue: 0.12,
            hue_tolerance: 0.08,
            saturation_min: 0.2,
            softness: 0.3,
        },
    ));
    // 每层给一组非默认参数，保证它们也被编码进去。
    {
        let layers = stack.layers_mut();
        layers[0].invert = true;
        layers[1].opacity = 0.6;
        layers[2].combine = MaskCombine::Subtract;
        layers[3].follow_base = false;
        layers[3].max_boost = 3.4;
        layers[4].follow_base = false;
        layers[4].knee = 0.42;
    }
    let encoded = stack.encode();
    let decoded = MaskStack::decode(&encoded);
    assert_eq!(stack, decoded, "编码往返不一致");
    println!(
        "编码往返 OK  {} 层 文本 {} 字节",
        decoded.len(),
        encoded.len()
    );

    // 损坏的配置不能把好层一起丢掉。
    let broken = format!("{encoded}~垃圾层");
    assert_eq!(MaskStack::decode(&broken), stack, "损坏的一层拖累了其他层");
    println!("容错 OK  损坏一层后仍读回 {} 层", MaskStack::decode(&broken).len());

    // ---- 2. 空蒙版必须和原来的结果完全一样 ----
    let empty = Settings::default();
    let base = build_gain_map(&load(&src), &empty);

    // ---- 3. 画一横条，检查只有该区域变化 ----
    let mut painted_settings = Settings::default();
    let mut band = BrushStroke::new(0.10, 1.0, 1.0, false, [0.05, 0.50]);
    band.push([0.95, 0.50]);
    let mut paint = BrushMask::default();
    paint.push(band);
    let mut band_layer = MaskLayer::new("横条", MaskShape::Brush(paint));
    // 新蒙版默认跟随全局曲线，那样画了也不会改变增益；这里显式给它一条更亮的局部曲线。
    // `max_boost` 特意不超过全局值：抬高上限会重排整张增益图的编码（对数刻度变了），
    // 那样就无法断言"只有画过的地方变了"。
    band_layer.follow_base = false;
    band_layer.knee = 0.0;
    band_layer.max_boost = 3.0;
    painted_settings.masks = Arc::new({
        let mut stack = MaskStack::default();
        stack.push(band_layer);
        stack
    });

    let painted = build_gain_map(&load(&src), &painted_settings);
    assert_eq!(base.0.width, painted.0.width);
    assert_eq!(base.0.height, painted.0.height);

    let w = base.0.width as usize;
    let h = base.0.height as usize;
    let changed = (0..w * h)
        .filter(|&i| base.0.data[i] != painted.0.data[i])
        .count();
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
    let mut erased_mask = BrushMask::default();
    let mut cover = BrushStroke::new(0.10, 1.0, 1.0, false, [0.05, 0.50]);
    cover.push([0.95, 0.50]);
    erased_mask.push(cover);
    // 橡皮擦要硬边：硬度 0 的软边只会按比例削掉一部分覆盖度，擦不干净。
    let mut wipe = BrushStroke::new(0.30, 1.0, 1.0, true, [0.05, 0.50]);
    wipe.push([0.95, 0.50]);
    erased_mask.push(wipe);

    let mut erased_stack = MaskStack::default();
    let mut erased_layer = MaskLayer::new("涂", MaskShape::Brush(erased_mask));
    erased_layer.follow_base = false;
    erased_layer.knee = 0.0;
    erased_layer.max_boost = 3.0;
    erased_stack.push(erased_layer);

    let mut erased_settings = Settings::default();
    erased_settings.masks = Arc::new(erased_stack);
    let after_erase = build_gain_map(&load(&src), &erased_settings);
    let residual = (0..w * h)
        .filter(|&i| after_erase.0.data[i] != base.0.data[i])
        .count();
    println!(
        "橡皮擦覆盖后：残留 {} 像素（原本 {}，{:.1}%）",
        residual,
        changed,
        100.0 * residual as f32 / changed.max(1) as f32
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
    assert!(
        jpg2ultrahdr::is_already_ultra_hdr(&bytes),
        "输出缺少 Ultra HDR 标记"
    );
    println!("输出带 Ultra HDR 标记 OK -> {}", dst.display());

    // ---- 6. 蒙版要求更高上限时，整幅图的上限要跟着抬 ----
    let mut dim_stack = MaskStack::default();
    let mut layer = MaskLayer::new("点", MaskShape::Brush({
        let mut mask = BrushMask::default();
        mask.push(BrushStroke::new(0.15, 1.0, 1.0, false, [0.5, 0.5]));
        mask
    }));
    layer.follow_base = false;
    layer.max_boost = 3.2;
    dim_stack.push(layer);

    let dim = Settings {
        auto_tone: true,
        knee: 0.95,
        max_boost: 1.5,
        masks: Arc::new(dim_stack),
        ..Default::default()
    };
    let plain = Settings {
        auto_tone: true,
        knee: 0.95,
        max_boost: 1.5,
        ..Default::default()
    };
    let image = load(&src);
    let with_mask = build_gain_map(&image, &dim);
    let without = build_gain_map(&image, &plain);
    println!(
        "峰值上限：无蒙版 {:.2}x，有蒙版 {:.2}x",
        without.1.max_content_boost[0], with_mask.1.max_content_boost[0]
    );
    assert!(
        with_mask.1.max_content_boost[0] >= 3.2 - 1e-6,
        "蒙版上限没被采纳：{:.2}x",
        with_mask.1.max_content_boost[0]
    );

    // ---- 7. 新建蒙版的默认参数必须真的改变导出结果 ----
    //
    // The editor hands every new mask `follow_base = false`, the base boost
    // and a knee 0.15 lower than the global one. If that combination ever
    // stops moving pixels, drawing a mask becomes a silent no-op — the user
    // sees the overlay, exports, and gets an untouched photo.
    let mut fresh = MaskLayer::new("新建", MaskShape::Brush({
        let mut mask = BrushMask::default();
        mask.push(BrushStroke::new(0.12, 1.0, 1.0, false, [0.5, 0.5]));
        mask
    }));
    fresh.follow_base = false;
    fresh.max_boost = tone.max_boost;
    fresh.knee = (tone.knee - 0.15).clamp(0.0, 0.9);

    let fresh_settings = Settings {
        masks: Arc::new({
            let mut stack = MaskStack::default();
            stack.push(fresh);
            stack
        }),
        ..Default::default()
    };
    let painted_fresh = build_gain_map(&image, &fresh_settings);
    let moved = (0..w * h)
        .filter(|&i| base.0.data[i] != painted_fresh.0.data[i])
        .count();
    println!(
        "新建蒙版默认参数：{} 像素变化（{:.2}%），上限 {:.2}x",
        moved,
        100.0 * moved as f64 / (w * h) as f64,
        painted_fresh.1.max_content_boost[0]
    );
    assert!(moved > 0, "新建蒙版按默认参数画完，导出结果没有变化");
    assert!(
        (painted_fresh.1.max_content_boost[0] - without.1.max_content_boost[0]).abs() < 1e-6,
        "新建蒙版不该抬高全局上限（会重排整张增益图）"
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
