//! 回归自检：没有蒙版时必须与历史结果逐字节一致；同一蒙版对不同分辨率的
//! 照片必须落在大致相同的位置（归一化坐标的意义所在）。
//!
//! 用法：`paint_regress <目录>`
//!
//! 位置检查走 `build_gain_map`（内存里的增益图），不走输出文件：JPEG 是有损的，
//! 重新压缩会把差异扩散到画过的区域之外几个块，看起来像"画歪了"。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jpg2ultrahdr::{
    BrushMask, BrushStroke, MaskLayer, MaskShape, MaskStack, Settings, build_gain_map, convert,
};

/// 0.1.2 发布版对 DSC_0227 自动模式的输出哈希。蒙版为空时必须仍然命中它。
const AUTO_HASH_0227: &str = "AD71D39AC5D610CEB6175535E1BD3062556C975BC0B8A69471B9AC22A022BC9F";

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut files = Vec::new();
    collect(&dir, 0, &mut files);
    files.sort();
    files.truncate(3);
    if files.is_empty() {
        println!("{} 下没有 JPEG", dir.display());
        return;
    }

    // 一个明显的斜向笔画 + 一个点，覆盖"折线"和"单点"两种情况。
    let mut mask = BrushMask::default();
    let mut slash = BrushStroke::new(0.05, 1.0, 1.0, false, [0.15, 0.20]);
    slash.push([0.35, 0.45]);
    slash.push([0.55, 0.70]);
    mask.push(slash);
    mask.push(BrushStroke::new(0.04, 1.0, 1.0, false, [0.80, 0.30]));

    let mut layer = MaskLayer::new("回归", MaskShape::Brush(mask));
    // 跟随全局的蒙版不会改变增益图，这里给它一条自己的曲线。上限必须留在全局
    // 曲线之内：一旦抬高 `capacity`，整张增益图的对数刻度都会变，所有像素都会
    // 变化，"只有画过的地方变了"就无法断言了。
    layer.follow_base = false;
    layer.knee = 0.0;
    layer.max_boost = 1.5;

    let empty = Settings {
        auto_tone: true,
        ..Default::default()
    };
    let painted = Settings {
        auto_tone: true,
        masks: Arc::new({
            let mut stack = MaskStack::default();
            stack.push(layer);
            stack
        }),
        ..Default::default()
    };

    for src in &files {
        let image = load(src);
        let plain_map = build_gain_map(&image, &empty).0;
        let paint_map = build_gain_map(&image, &painted).0;
        let (w, h) = (paint_map.width, paint_map.height);

        let differing = plain_map
            .data
            .iter()
            .zip(paint_map.data.iter())
            .filter(|(a, b)| a != b)
            .count();
        let rows = changed_rows(&plain_map.data, &paint_map.data, w, h);
        let lo = rows.0 as f32 / h as f32;
        let hi = rows.1 as f32 / h as f32;
        let cols = changed_cols(&plain_map.data, &paint_map.data, w, h);

        println!(
            "{}\n  增益图 {w}x{h}  变化 {:.2}%  行 {lo:.3}..{hi:.3}  列 {:.3}..{:.3}",
            src.file_name().unwrap().to_string_lossy(),
            100.0 * differing as f32 / plain_map.data.len() as f32,
            cols.0 as f32 / w as f32,
            cols.1 as f32 / w as f32,
        );

        assert!(differing > 0, "蒙版没有影响增益图");
        // 笔画 v=0.20..0.70，半径 0.05 倍宽（纵向 0.05*w/h 的余量）。
        assert!(lo < 0.25, "起始行 {lo:.3} 偏下，笔画在 0.20");
        assert!(hi > 0.65, "结束行 {hi:.3} 偏上，笔画到 0.70");
        assert!(hi < 0.85, "结束行 {hi:.3} 过头了，不该擦到 0.70 以下太多");
    }

    // ---- 空蒙版必须与历史逐字节一致 ----
    let sample = files
        .iter()
        .find(|p| p.file_name().unwrap().to_string_lossy() == "DSC_0227.jpg");
    if let Some(src) = sample {
        let tmp = std::env::temp_dir().join("jpg2ultrahdr_paint_hash.jpg");
        convert(src, &tmp, &empty).expect("转换失败");
        let hash = sha256_hex(&std::fs::read(&tmp).expect("读回失败"));
        println!("\nDSC_0227 自动模式 SHA256 {hash}");
        assert_eq!(hash, AUTO_HASH_0227, "空蒙版改动了历史输出");
        println!("空蒙版与 0.1.2 逐字节一致");
    } else {
        println!("\n（样本里没有 DSC_0227.jpg，跳过哈希比对）");
    }

    println!("\n回归通过");
}

fn changed_rows(a: &[u8], b: &[u8], w: u32, h: u32) -> (u32, u32) {
    let stride = w as usize;
    let mut first = None;
    let mut last = 0;
    for y in 0..h as usize {
        if (0..stride).any(|x| a[y * stride + x] != b[y * stride + x]) {
            first.get_or_insert(y as u32);
            last = y as u32;
        }
    }
    (first.unwrap_or(0), last)
}

fn changed_cols(a: &[u8], b: &[u8], w: u32, h: u32) -> (u32, u32) {
    let stride = w as usize;
    let mut first = None;
    let mut last = 0;
    for x in 0..stride {
        if (0..h as usize).any(|y| a[y * stride + x] != b[y * stride + x]) {
            first.get_or_insert(x as u32);
            last = x as u32;
        }
    }
    (first.unwrap_or(0), last)
}

fn load(path: &Path) -> ultrahdr_core::RawImage {
    let data = std::fs::read(path).expect("读不到源文件");
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&data));
    decoder.decode_headers().expect("解码头失败");
    let info = decoder.info().expect("拿不到图像信息");
    let pixels = decoder.decode().expect("解码失败");
    let (width, height) = (info.width as u32, info.height as u32);
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

fn sha256_hex(data: &[u8]) -> String {
    // 只为了比对哈希，自己实现一份最小 SHA-256，避免给 bin 增加依赖。
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bits = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|word| format!("{word:08X}")).collect()
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, depth + 1, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
                out.push(path);
            }
        }
    }
}
