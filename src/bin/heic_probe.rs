//! Prints what heifer sees in a HEIC/HEIF file: container headroom, metadata, XMP.

use std::path::PathBuf;

fn main() {
    let path: PathBuf = std::env::args()
        .nth(1)
        .expect("用法: heic_probe <file.heic>")
        .into();
    let data = std::fs::read(&path).expect("读取失败");
    println!("文件 {}  {} 字节", path.display(), data.len());

    match heifer::decode_hdr(&data) {
        Ok(hdr) => println!(
            "decode_hdr OK  {}x{}  headroom {:.4}",
            hdr.width, hdr.height, hdr.headroom
        ),
        Err(e) => println!("decode_hdr 失败: {e}"),
    }

    match heifer::read_metadata(&data) {
        Ok(m) => {
            println!(
                "metadata: exif={:?} xmp={:?} icc={:?}",
                m.exif.as_ref().map(|v| v.len()),
                m.xmp.as_ref().map(|v| v.len()),
                m.icc_profile.as_ref().map(|v| v.len())
            );
            if let Some(xmp) = &m.xmp {
                let text = String::from_utf8_lossy(xmp);
                println!("--- XMP ---\n{text}\n-----------");
            }
        }
        Err(e) => println!("metadata 失败: {e}"),
    }
}
