//! 边界与健壮性测试：各种尺寸、灰阶、已转换文件、损坏文件。

use std::path::{Path, PathBuf};

use jpg2ultrahdr::{Settings, convert, is_already_ultra_hdr, output_path};

fn make_jpeg(path: &Path, w: u32, h: u32, mode: &str) {
    let img = match mode {
        "gray" => image::RgbImage::from_fn(w, h, |x, y| {
            let v = ((x + y) % 256) as u8;
            image::Rgb([v, v, v])
        }),
        "gradient" => image::RgbImage::from_fn(w, h, |x, _| {
            image::Rgb([(x % 256) as u8, 128, (255 - (x % 256)) as u8])
        }),
        _ => image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 200])
        }),
    };
    img.save(path).expect("写入测试图失败");
}

fn main() {
    let dir = PathBuf::from(r"D:\Photos\Ajpg2hdr\_out\edge");
    std::fs::create_dir_all(&dir).unwrap();

    let cases: [(u32, u32, &str); 8] = [
        (1, 1, "flat"),
        (3, 5, "flat"),
        (16, 16, "gradient"),
        (17, 33, "gradient"),
        (320, 240, "gray"),
        (1023, 767, "gradient"),
        (2048, 1, "gradient"),
        (1, 2048, "gradient"),
    ];

    let settings = Settings::default();
    let mut pass = 0;
    let mut fail = 0;

    for (w, h, mode) in cases {
        let src = dir.join(format!("case_{w}x{h}_{mode}.jpg"));
        let dst = dir.join(format!("case_{w}x{h}_{mode}_hdr.jpg"));
        make_jpeg(&src, w, h, mode);
        match convert(&src, &dst, &settings) {
            Ok(r) => {
                let ok_dims = r.width == w && r.height == h;
                let data = std::fs::read(&dst).unwrap();
                let info = ultrajpeg::inspect(&data).unwrap();
                let has_gain = info.gain_map_jpeg_len.is_some();
                if ok_dims && has_gain {
                    println!("✓ {w}x{h} {mode:9} -> 增益图 {}x{} {} 字节", r.gain_width, r.gain_height, r.bytes);
                    pass += 1;
                } else {
                    println!("✗ {w}x{h} {mode:9} 尺寸或增益图异常");
                    fail += 1;
                }
            }
            Err(e) => {
                println!("✗ {w}x{h} {mode:9} 失败: {e:#}");
                fail += 1;
            }
        }
    }

    // 重复转换：结果应被识别为 Ultra HDR 并拒绝
    let first = dir.join("case_320x240_gray_hdr.jpg");
    if first.exists() {
        let bytes = std::fs::read(&first).unwrap();
        if is_already_ultra_hdr(&bytes) {
            println!("✓ 可识别已转换的 Ultra HDR 文件");
        } else {
            println!("✗ 未能识别已转换的 Ultra HDR 文件");
            fail += 1;
        }
        match convert(&first, &dir.join("again.jpg"), &settings) {
            Ok(_) => {
                println!("✗ 重复转换未被拒绝");
                fail += 1;
            }
            Err(e) => {
                println!("✓ 重复转换被拒绝: {e}");
                pass += 1;
            }
        }
    }

    // 非 JPEG 内容
    let bogus = dir.join("bogus.jpg");
    std::fs::write(&bogus, b"not a jpeg at all").unwrap();
    match convert(&bogus, &dir.join("bogus_hdr.jpg"), &settings) {
        Ok(_) => {
            println!("✗ 损坏文件未被拒绝");
            fail += 1;
        }
        Err(e) => {
            println!("✓ 损坏文件被拒绝: {e:#}");
            pass += 1;
        }
    }

    // 输出命名
    let out = output_path(Path::new(r"D:\x\photo.JPG"), None);
    println!(
        "{} 输出名 = {}",
        if out.ends_with("photo_hdr.jpg") { "✓" } else { "✗" },
        out.display()
    );

    println!("\n通过 {pass} / 失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
