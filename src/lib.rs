//! Core conversion: ordinary SDR JPEG -> Ultra HDR JPEG.
//!
//! An Ultra HDR file is a normal SDR JPEG (the "primary") plus a gain map that
//! tells HDR-capable viewers how much brighter each region should be rendered.
//! A plain SDR photo carries no HDR information, so the gain map has to be
//! invented: we expand the highlights by a smooth curve derived from the
//! luminance of the primary itself.
//!
//! The primary is re-encoded (ultrajpeg always re-encodes from pixels), so the
//! gain map is computed against exactly the pixels that end up in the file.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use ultrahdr_core::color::{gamut_conversion_matrix, luma_coefficients, srgb_eotf, srgb_oetf};
use ultrahdr_core::{ColorGamut, ColorTransfer, GainMapMetadata, PixelFormat, RawImage};
use ultrajpeg::{
    ChromaSubsampling, ColorMetadata, CompressionEffort, EncodeOptions, GainMapBundle,
    PrimaryMetadata,
};

/// ISO 21496-1 gain map offset (same value the reference implementation uses).
const OFFSET: f32 = 1.0 / 64.0;

/// Namespaces that mark a JPEG as already carrying a gain map.
///
/// The Google *container* namespace is deliberately NOT in this list: motion
/// photos (Live Photos) use it to describe a still + video container without
/// any gain map, so matching it produced false "already Ultra HDR" rejections.
/// Only the gain-map namespace and the ISO 21496-1 marker are specific enough.
const ULTRA_HDR_MARKERS: [&[u8]; 2] = [
    b"http://ns.adobe.com/hdr-gain-map/1.0/",
    b"urn:iso:std:iso:ts:21496:-1",
];

#[derive(Debug, Clone)]
pub struct Settings {
    /// Quality of the SDR primary JPEG (1..=100).
    pub primary_quality: u8,
    /// Quality of the gain map JPEG (1..=100).
    pub gainmap_quality: u8,
    /// Gain map resolution divisor: 1 = full, 2 = half, 4 = quarter.
    pub scale: u32,
    /// Peak brightness multiplier the gain map can reach.
    pub max_boost: f32,
    /// Perceptual luminance where the highlight expansion starts (0..1).
    pub knee: f32,
    /// Write a progressive JPEG.
    pub progressive: bool,
    /// Convert the primary from BT.709 to Display P3.
    pub to_display_p3: bool,
    /// Copy the source EXIF (orientation, capture time, camera) into the output.
    pub keep_exif: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            primary_quality: 92,
            gainmap_quality: 90,
            scale: 2,
            max_boost: 3.0,
            knee: 0.55,
            progressive: true,
            to_display_p3: false,
            keep_exif: true,
        }
    }
}

pub struct Report {
    pub width: u32,
    pub height: u32,
    pub gain_width: u32,
    pub gain_height: u32,
    pub bytes: usize,
    pub elapsed_ms: u128,
    pub peak_boost: f32,
    pub gamut: &'static str,
}

fn gamut_name(gamut: ColorGamut) -> &'static str {
    match gamut {
        ColorGamut::Bt709 => "sRGB / BT.709",
        ColorGamut::DisplayP3 => "Display P3",
        ColorGamut::Bt2100 => "BT.2100",
    }
}

/// True when the file already looks like an Ultra HDR / gain map container.
///
/// `ultrajpeg::inspect` is deliberately not used here: it splits a JPEG into
/// codestreams with `find_jpeg_boundaries`, which mistakes the EXIF thumbnail
/// inside ordinary camera JPEGs for a second image.
pub fn is_already_ultra_hdr(data: &[u8]) -> bool {
    ULTRA_HDR_MARKERS
        .iter()
        .any(|needle| data.windows(needle.len()).any(|w| w == *needle))
}

/// Everything needed from the source JPEG before re-encoding it.
struct Source {
    pixels: RawImage,
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
}

/// Decode the SDR source ourselves.
///
/// `ultrajpeg::decode` cannot be used for camera JPEGs: it treats the embedded
/// EXIF thumbnail as a second codestream and then hands a truncated primary to
/// the JPEG decoder. zune-jpeg decodes the file as a whole and also hands us
/// the original EXIF and ICC payloads.
fn decode_source(data: &[u8]) -> Result<Source> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(data));
    decoder
        .decode_headers()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| "读取 JPEG 头信息失败")?;

    let exif = decoder.exif().cloned();
    let icc = decoder.icc_profile();
    let (width, height) = match decoder.info() {
        Some(info) => (info.width as u32, info.height as u32),
        None => bail!("无法读取图像尺寸"),
    };

    let pixels = decoder
        .decode()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| "解码 JPEG 像素失败")?;

    let expected = (width as usize) * (height as usize) * 3;
    if pixels.len() < expected {
        bail!(
            "解码结果不完整：期望 {} 字节，实际 {}",
            expected,
            pixels.len()
        );
    }
    let mut pixels = pixels;
    pixels.truncate(expected);

    let image = RawImage::from_data(
        width,
        height,
        PixelFormat::Rgb8,
        ColorGamut::Bt709,
        ColorTransfer::Srgb,
        pixels,
    )
    .with_context(|| "构建图像缓冲区失败")?;

    Ok(Source {
        pixels: image,
        exif,
        icc,
    })
}

/// Convert one file. `dst` is written on success.
pub fn convert(src: &Path, dst: &Path, settings: &Settings) -> Result<Report> {
    let started = Instant::now();
    let data = std::fs::read(src).with_context(|| format!("读取 {} 失败", src.display()))?;

    if is_already_ultra_hdr(&data) {
        bail!("源文件已经是 Ultra HDR 图像");
    }

    let source = decode_source(&data)?;
    let mut primary = source.pixels;

    // ---- colour handling -------------------------------------------------
    // The source pixels are passed through untouched, so whatever ICC profile
    // the camera wrote still describes them: carry it over verbatim and write
    // no explicit gamut marker, because the ICC is the authority. Only when
    // the file has no ICC do we have to state a gamut ourselves.
    let mut gamut = ColorGamut::Bt709;
    let mut color = ColorMetadata::default();

    if settings.to_display_p3 {
        // The pixels are being moved, so the old profile no longer applies.
        convert_primary_gamut(&mut primary, ColorGamut::Bt709, ColorGamut::DisplayP3);
        gamut = ColorGamut::DisplayP3;
        color = ColorMetadata::display_p3();
    } else if let Some(icc) = source.icc {
        color.icc_profile = Some(icc);
    } else {
        color.gamut = Some(ColorGamut::Bt709);
        color.transfer = Some(ColorTransfer::Srgb);
    }
    primary.gamut = gamut;
    primary.transfer = ColorTransfer::Srgb;

    let exif = if settings.keep_exif { source.exif } else { None };

    // ---- gain map --------------------------------------------------------
    let (gain_image, gain_metadata) = build_gain_map(&primary, settings);
    let (gain_width, gain_height) = (gain_image.width, gain_image.height);
    let peak_boost = gain_metadata.max_content_boost[0];

    let options = EncodeOptions {
        quality: settings.primary_quality.clamp(1, 100),
        progressive: settings.progressive,
        compression: CompressionEffort::Balanced,
        chroma_subsampling: ChromaSubsampling::Yuv420,
        primary_metadata: PrimaryMetadata { color, exif },
        ultra_hdr_metadata_emission: Default::default(),
        gain_map: Some(GainMapBundle {
            image: gain_image,
            metadata: gain_metadata,
            quality: settings.gainmap_quality.clamp(1, 100),
            progressive: false,
            compression: CompressionEffort::Balanced,
        }),
    };

    let output = ultrajpeg::encode(&primary, &options).with_context(|| "编码 Ultra HDR 失败")?;

    // Cheap sanity check: the container must advertise a gain map.
    let check = ultrajpeg::inspect(&output).with_context(|| "回读校验失败")?;
    if check.gain_map_jpeg_len.is_none() {
        bail!("输出文件缺少增益图");
    }

    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    std::fs::write(dst, &output).with_context(|| format!("写入 {} 失败", dst.display()))?;

    Ok(Report {
        width: primary.width,
        height: primary.height,
        gain_width,
        gain_height,
        bytes: output.len(),
        elapsed_ms: started.elapsed().as_millis(),
        peak_boost,
        gamut: gamut_name(gamut),
    })
}

/// Build a single-channel gain map plus matching ISO 21496-1 metadata.
///
/// Each gain map sample covers a `scale x scale` block of the primary and is
/// taken from the block centre. The gain follows
///
/// ```text
/// m    = smoothstep(knee, 1, srgb_oetf(luminance))
/// gain = 1 + (max_boost - 1) * m
/// byte = 255 * ln(gain) / ln(max_boost)
/// ```
///
/// which is exactly the encoding `apply_gainmap` decodes, so a viewer
/// reproducing the curve lands on the intended brightness: midtones and
/// shadows stay untouched, highlights expand up to `max_boost`.
pub fn build_gain_map(primary: &RawImage, settings: &Settings) -> (RawImage, GainMapMetadata) {
    let width = primary.width;
    let height = primary.height;
    let scale = settings.scale.max(1);
    let gain_width = width.div_ceil(scale);
    let gain_height = height.div_ceil(scale);

    let max_boost = settings.max_boost.clamp(1.0, 16.0);
    let log_max = max_boost.ln();
    let knee = settings.knee.clamp(0.0, 0.999);
    let span = (1.0 - knee).max(1e-6);
    let luma = luma_coefficients(primary.gamut);

    let mut data = vec![0u8; (gain_width as usize) * (gain_height as usize)];

    data.par_chunks_mut(gain_width as usize)
        .enumerate()
        .for_each(|(gy, row)| {
            let sy = ((gy as u32) * scale + scale / 2).min(height - 1);
            let base = sy * primary.stride;
            for (gx, slot) in row.iter_mut().enumerate() {
                let sx = ((gx as u32) * scale + scale / 2).min(width - 1);
                let idx = (base + sx * 3) as usize;
                let luminance = luma[0] * srgb_eotf(primary.data[idx] as f32 / 255.0)
                    + luma[1] * srgb_eotf(primary.data[idx + 1] as f32 / 255.0)
                    + luma[2] * srgb_eotf(primary.data[idx + 2] as f32 / 255.0);

                let level = srgb_oetf(luminance.clamp(0.0, 1.0));
                let mut t = ((level - knee) / span).clamp(0.0, 1.0);
                t = t * t * (3.0 - 2.0 * t); // smoothstep
                let gain = 1.0 + (max_boost - 1.0) * t;

                *slot = if log_max > 1e-6 {
                    ((gain.ln() / log_max) * 255.0).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                };
            }
        });

    let image = RawImage::from_data(
        gain_width,
        gain_height,
        PixelFormat::Gray8,
        ColorGamut::Bt709,
        ColorTransfer::Linear,
        data,
    )
    .expect("gain map dimensions are derived from the primary");

    let metadata = GainMapMetadata {
        max_content_boost: [max_boost; 3],
        min_content_boost: [1.0; 3],
        gamma: [1.0; 3],
        offset_sdr: [OFFSET; 3],
        offset_hdr: [OFFSET; 3],
        hdr_capacity_min: 1.0,
        hdr_capacity_max: max_boost,
        use_base_color_space: true,
    };

    (image, metadata)
}

/// Re-tag the primary by actually moving its pixels between primaries, so the
/// visible colours stay the same on a colour-managed viewer.
fn convert_primary_gamut(image: &mut RawImage, from: ColorGamut, to: ColorGamut) {
    let matrix = gamut_conversion_matrix(from, to);
    image.data.par_chunks_exact_mut(3).for_each(|px| {
        let linear = [
            srgb_eotf(px[0] as f32 / 255.0),
            srgb_eotf(px[1] as f32 / 255.0),
            srgb_eotf(px[2] as f32 / 255.0),
        ];
        let converted = matrix.transform(linear);
        for channel in 0..3 {
            px[channel] = (srgb_oetf(converted[channel].clamp(0.0, 1.0)) * 255.0).round() as u8;
        }
    });
}

/// `photo.jpg` -> `photo_hdr.jpg`, either beside the source or in `out_dir`.
pub fn output_path(src: &Path, out_dir: Option<&Path>) -> std::path::PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned());
    let name = format!("{stem}_hdr.jpg");
    match out_dir {
        Some(dir) => dir.join(name),
        None => src.with_file_name(name),
    }
}
