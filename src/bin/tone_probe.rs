//! 打印自动曲线为每张图片推断出的参数，用来和仿真脚本对账。
//! 用 `cargo run --release --bin tone_probe -- <目录或文件>...`。

use std::path::{Path, PathBuf};

use jpg2ultrahdr as convert;
use ultrahdr_core::{ColorGamut, ColorTransfer, PixelFormat, RawImage};

fn decode(path: &Path) -> RawImage {
    let bytes = std::fs::read(path).expect("读取失败");
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes));
    decoder.decode_headers().expect("读取头失败");
    let (width, height) = decoder.info().map(|i| (i.width as u32, i.height as u32)).expect("无尺寸");
    let mut pixels = decoder.decode().expect("解码失败");
    pixels.truncate((width as usize) * (height as usize) * 3);
    RawImage::from_data(width, height, PixelFormat::Rgb8, ColorGamut::Bt709, ColorTransfer::Srgb, pixels)
        .expect("构建缓冲区失败")
}

fn jpegs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            jpegs(&path, out);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")) {
            out.push(path);
        }
    }
}

fn main() {
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        let path = PathBuf::from(arg);
        if path.is_dir() {
            jpegs(&path, &mut files);
        } else {
            files.push(path);
        }
    }
    files.sort();

    println!("{:<28} {:>6} {:>6} {:>6} {:>6}", "file", "knee", "boost", "top", "stops");
    for path in files {
        let image = decode(&path);
        let tone = convert::auto_tone(&image);
        let name = path.file_name().unwrap().to_string_lossy();
        println!(
            "{:<28} {:>6.2} {:>6.2} {:>6.2} {:>6.2}",
            name, tone.knee, tone.max_boost, tone.top_level, tone.top_stops,
        );
    }
}
