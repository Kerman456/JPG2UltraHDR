//! 诊断：定位 ultrajpeg::decode 在真实照片上失败的原因。

use std::path::PathBuf;

fn main() {
    let src: PathBuf = std::env::args().nth(1).expect("缺少源文件").into();
    let bytes = std::fs::read(&src).expect("读取失败");
    println!("文件 {} 字节", bytes.len());

    match ultrajpeg::inspect_container_layout(&bytes) {
        Ok(layout) => println!("layout: {:?}", layout),
        Err(e) => println!("layout 失败: {e}"),
    }

    match ultrajpeg::inspect(&bytes) {
        Ok(i) => println!(
            "inspect 成功: primary={} gain={:?}",
            i.primary_jpeg_len, i.gain_map_jpeg_len
        ),
        Err(e) => println!("inspect 失败: {e}"),
    }

    match ultrajpeg::decode(&bytes) {
        Ok(d) => println!("decode 成功: {}x{}", d.image.width, d.image.height),
        Err(e) => println!("decode 失败: {e}"),
    }

    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&bytes));
    match dec.decode() {
        Ok(px) => println!("zune-jpeg 直接解码成功: {:?} {} 字节", dec.output_buffer_size(), px.len()),
        Err(e) => println!("zune-jpeg 直接解码失败: {e:?}"),
    }

    // 扫描 marker 结构
    let mut i = 2usize;
    let mut count = 0;
    while i + 4 <= bytes.len() && count < 40 {
        if bytes[i] != 0xFF {
            println!("偏移 {i} 不是 marker 起点（{:#04x}）", bytes[i]);
            break;
        }
        let marker = bytes[i + 1];
        if marker == 0xD9 {
            println!("EOI at {i}");
            break;
        }
        if marker == 0xDA {
            println!("SOS at {i}");
            break;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let preview: String = bytes[i + 4..(i + 4 + 12).min(bytes.len())]
            .iter()
            .map(|&b| if (32..127).contains(&b) { b as char } else { '.' })
            .collect();
        println!("marker FF{marker:02X} at {i} len {len} 前缀 {preview}");
        i += 2 + len;
        count += 1;
    }
}
