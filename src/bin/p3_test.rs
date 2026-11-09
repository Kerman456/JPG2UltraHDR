//! P3 转换路径的冒烟测试：直接调用库，验证 ICC 与像素都按预期变化。

use std::path::PathBuf;

use jpg2ultrahdr::{Settings, convert};

fn main() {
    let src: PathBuf = std::env::args()
        .nth(1)
        .expect("用法: p3_test <src.jpg> <dst.jpg>")
        .into();
    let dst: PathBuf = std::env::args()
        .nth(2)
        .expect("用法: p3_test <src.jpg> <dst.jpg>")
        .into();

    let settings = Settings {
        to_display_p3: true,
        ..Default::default()
    };
    match convert(&src, &dst, &settings) {
        Ok(report) => {
            println!(
                "P3 转换完成: {}x{} 色域 {} 峰值 {:.2}x 体积 {} 字节 用时 {} ms",
                report.width, report.height, report.gamut, report.peak_boost,
                report.bytes, report.elapsed_ms
            );
            let data = std::fs::read(&dst).unwrap();
            let info = ultrajpeg::inspect(&data).unwrap();
            let icc = info.primary_metadata.color.icc_profile;
            println!(
                "ICC: {:?} 字节, gamut={:?} transfer={:?}",
                icc.as_ref().map(|v| v.len()),
                info.primary_metadata.color.gamut,
                info.primary_metadata.color.transfer
            );
        }
        Err(e) => {
            println!("P3 转换失败: {e:#}");
            std::process::exit(1);
        }
    }
}
