//! 只读检查一个 Ultra HDR JPEG：容器结构、增益图统计、按不同显示亮度重建 HDR。

use ultrahdr_core::HdrOutputFormat;

fn main() {
    let path = std::env::args().nth(1).expect("用法: inspect_hdr <file.jpg>");
    let data = std::fs::read(&path).expect("读取文件失败");

    let layout = ultrajpeg::inspect_container_layout(&data).expect("解析容器失败");
    println!("容器类型: {:?}", layout.kind);
    println!("码流数: {}", layout.codestreams.len());
    for (i, cs) in layout.codestreams.iter().enumerate() {
        println!("  码流 {i}: 偏移 {} 长度 {}", cs.offset, cs.len);
    }

    let inspected = ultrajpeg::inspect(&data).expect("inspect 失败");
    println!(
        "主图 {} 字节, 增益图 {:?} 字节",
        inspected.primary_jpeg_len,
        inspected.gain_map_jpeg_len
    );
    let ultra = inspected.ultra_hdr.as_ref().expect("缺少 Ultra HDR 元数据");
    println!("有 XMP 容器元数据: {}", ultra.xmp.is_some());
    println!("有 ISO 21496-1: {}", ultra.iso_21496_1.is_some());
    if let Some(gm) = &ultra.gain_map_metadata {
        println!(
            "增益图元数据: max_boost={:?} min_boost={:?} 容量 {:.3}..{:.3} gamma={:?}",
            gm.max_content_boost, gm.min_content_boost, gm.hdr_capacity_min, gm.hdr_capacity_max, gm.gamma
        );
    }

    let decoded = ultrajpeg::decode(&data).expect("解码失败");
    println!(
        "解码得到 {}x{} 主图",
        decoded.image.width, decoded.image.height
    );
    match &decoded.gain_map {
        Some(gm) => {
            let g = &gm.image;
            println!(
                "增益图 {}x{} 格式 {:?}",
                g.width, g.height, g.format
            );
            let mut zeros = 0usize;
            let mut max = 0u8;
            let mut sum = 0u64;
            for &b in &g.data {
                if b == 0 {
                    zeros += 1;
                }
                max = max.max(b);
                sum += b as u64;
            }
            let total = g.data.len().max(1);
            println!(
                "增益图分布: 0 值 {:.1}% 最大 {} 均值 {:.1}",
                zeros as f64 * 100.0 / total as f64,
                max,
                sum as f64 / total as f64
            );
        }
        None => println!("没有增益图"),
    }

    // 按不同显示亮度重建，确认画面确实随 boost 变亮。
    for boost in [1.0f32, 1.5, 2.0] {
        match decoded.reconstruct_hdr(boost, HdrOutputFormat::LinearFloat) {
            Ok(hdr) => {
                let mut sum = 0.0f64;
                let mut n = 0usize;
                for px in hdr.data.chunks_exact(4) {
                    let r = f32::from_le_bytes([px[0], px[1], px[2], px[3]]) as f64;
                    sum += r;
                    n += 1;
                }
                println!("  display_boost={boost:.2} -> 平均线性亮度 {:.4}", sum / n.max(1) as f64);
            }
            Err(e) => println!("  display_boost={boost:.2} -> 重建失败: {e}"),
        }
    }
}
