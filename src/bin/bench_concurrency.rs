//! Measure how well a batch scales with worker count.
//!
//! The GUI runs `concurrency()` workers in parallel, each of which internally
//! saturates several cores. If one conversion already uses most of the machine
//! there is little headroom for a GPU to win; if a single conversion leaves
//! cores idle, the bottleneck is inside the encoder and a hardware encoder is
//! worth considering.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use jpg2ultrahdr::{OutputFormat, Settings, convert, output_path};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let src = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\Photos\0425\照片\未命名导出\DSC_0227.jpg"));
    let format = match args.get(1).map(String::as_str) {
        Some("heic") => OutputFormat::Heic,
        _ => OutputFormat::Jpeg,
    };
    let out_dir = PathBuf::from(r"D:\Photos\Ajpg2hdr\_bench");

    // Optional third argument: comma-separated worker counts, so a caller can
    // sample CPU during one specific stage instead of the whole sweep.
    let counts: Vec<usize> = match args.get(2) {
        Some(list) => list.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        None => vec![1, 2, 4, 8],
    };

    for workers in counts {
        let total = workers * 2;
        let t = Instant::now();
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= total {
                            break;
                        }
                        let settings = Settings {
                            format,
                            ..Settings::default()
                        };
                        let dst = output_path(&src, Some(&out_dir), format)
                            .with_file_name(format!("w{index}_hdr.{}", format.extension()));
                        convert(&src, &dst, &settings).expect("转换失败");
                    }
                });
            }
        });
        let elapsed = t.elapsed().as_secs_f64();
        println!(
            "workers={workers:2}  images={total:2}  wall={elapsed:6.2}s  \
             per-image={:5.2}s  throughput={:.2} img/s",
            elapsed / total as f64,
            total as f64 / elapsed
        );
    }

    Ok(())
}
