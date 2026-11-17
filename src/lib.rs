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

// ---- automatic tone curve ------------------------------------------------
//
// A fixed knee/max_boost pair cannot suit every picture: a dim interior tops
// out at a perceptual level around 0.55 while a sunlit landscape reaches 0.97,
// so the same knee either drags most of the midtones into the boost or leaves
// the highlights untouched.
//
// Instead of guessing two numbers, the rule pins the *result* and derives the
// numbers from it:
//
//   * the brightest content of the picture (P99.9 of the perceptual level)
//     should gain `AUTO_TARGET_STOPS` stops in the HDR rendering;
//   * the gain map may never exceed `AUTO_BOOST_MAX`, which limits how steep
//     the shoulder is allowed to be.
//
// Invert that limit and the knee falls out: the shoulder must still be able to
// reach the target at the top of the picture, so the knee cannot sit *above*
// `(P99.9 - r) / (1 - r)`, where `r` is the argument whose smoothstep equals the
// fraction of the available boost the target needs. Taking the *lower* of that
// bound and P95 keeps the shoulder clear of the bulk of the image.

/// Gain (in stops) applied to the brightest content by the automatic curve.
const AUTO_TARGET_STOPS: f32 = 1.5;
/// Upper bound for the automatic peak gain; also caps the shoulder steepness.
const AUTO_BOOST_MAX: f32 = 4.0;
/// Lower bound so a picture with almost no range still gets a sane curve.
const AUTO_BOOST_MIN: f32 = 1.5;
/// The knee is never placed below this level: below it the curve would pull in
/// midtones, which reads as a washed-out global brightening rather than as
/// recovered highlight detail.
const AUTO_KNEE_MIN: f32 = 0.40;
/// ...and never above this, or the shoulder would sit on the brightest few
/// pixels and recover nothing visible.
const AUTO_KNEE_MAX: f32 = 0.92;
/// How many pixels the histogram samples; larger than this gains nothing.
const AUTO_SAMPLES: u64 = 250_000;

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
    /// Derive [`Self::max_boost`] and [`Self::knee`] from each picture instead
    /// of using the two values above. Keeps any picture from being pushed
    /// either into the midtones or past its own highlights.
    pub auto_tone: bool,
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
            auto_tone: true,
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

/// Resolved highlight expansion, either typed in by the user or derived from
/// the picture by [`auto_tone`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneParams {
    pub knee: f32,
    pub max_boost: f32,
    /// Perceptual level of the brightest content (P99.9 of the histogram).
    pub top_level: f32,
    /// Stops the curve actually adds at [`Self::top_level`].
    pub top_stops: f32,
}

/// Derive the highlight curve from the picture's own luminance histogram.
///
/// See the constants at the top of this module for the reasoning. The result is
/// deterministic for a given picture, so converting the same file twice yields
/// the same gain map.
pub fn auto_tone(primary: &RawImage) -> ToneParams {
    let (p95, p999) = top_percentiles(primary);

    // P95 first: starting the shoulder above 95% of the picture keeps the
    // expansion confined to the top of the range.
    let reach = smoothstep_inv(need_t());
    let mut knee = p95.min((p999 - reach) / (1.0 - reach));
    knee = knee.clamp(AUTO_KNEE_MIN, AUTO_KNEE_MAX);
    // Never let the shoulder start above the tip of the histogram.
    knee = knee.min((p999 - 0.02).max(AUTO_KNEE_MIN));

    let t = smoothstep((p999 - knee) / (1.0 - knee).max(1e-6));
    // A picture whose brightest content still sits below the shoulder (a very
    // dim, underexposed frame) has no highlight headroom to expand. Lifting its
    // top few percent anyway would paint a bright patch into an almost black
    // image, so leave it alone and emit an empty gain map.
    if t <= 1e-4 {
        return ToneParams {
            knee,
            max_boost: AUTO_BOOST_MIN,
            top_level: p999,
            top_stops: 0.0,
        };
    }

    let target = 2f32.powf(AUTO_TARGET_STOPS);
    let max_boost = (1.0 + (target - 1.0) / t).clamp(AUTO_BOOST_MIN, AUTO_BOOST_MAX);

    ToneParams {
        knee,
        max_boost,
        top_level: p999,
        top_stops: (1.0 + (max_boost - 1.0) * t).log2(),
    }
}

/// Fraction of the available boost that [`AUTO_TARGET_STOPS`] needs when the
/// curve is capped at [`AUTO_BOOST_MAX`].
fn need_t() -> f32 {
    let target = 2f32.powf(AUTO_TARGET_STOPS);
    ((target - 1.0) / (AUTO_BOOST_MAX - 1.0)).clamp(0.0, 1.0)
}

/// Solve `smoothstep(r) == need_t()` once; the result sets the lowest knee the
/// capped curve can still reach the target from.
fn smoothstep_inv(y: f32) -> f32 {
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if smoothstep(mid) < y {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Perceptual levels at 95% and 99.9% of the picture, on the same axis the gain
/// curve thresholds on.
///
/// The histogram is built from a strided sample: the percentiles only need the
/// shape of the distribution, and a 20 MP photo would otherwise cost a full
/// pass over 60 MB for the same answer.
fn top_percentiles(primary: &RawImage) -> (f32, f32) {
    let mut histogram = [0u32; 256];
    let luma = luma_coefficients(primary.gamut);
    let total = (primary.width as usize) * (primary.height as usize);
    // 250k samples: a 20 MP picture is sampled every 80th pixel.
    let step = (total / AUTO_SAMPLES as usize).max(1);
    let stride = primary.stride as usize;

    for index in (0..total).step_by(step) {
        let y = index / primary.width as usize;
        let x = index % primary.width as usize;
        let offset = y * stride + x * 3;
        let luminance = luma[0] * srgb_eotf(primary.data[offset] as f32 / 255.0)
            + luma[1] * srgb_eotf(primary.data[offset + 1] as f32 / 255.0)
            + luma[2] * srgb_eotf(primary.data[offset + 2] as f32 / 255.0);
        let level = srgb_oetf(luminance.clamp(0.0, 1.0));
        let bin = (level * 255.0).round().clamp(0.0, 255.0) as usize;
        histogram[bin] += 1;
    }

    let count: u64 = histogram.iter().map(|&c| u64::from(c)).sum();
    if count == 0 {
        return (1.0, 1.0);
    }
    (percentile(&histogram, count, 0.95), percentile(&histogram, count, 0.999))
}

fn percentile(histogram: &[u32; 256], total: u64, fraction: f64) -> f32 {
    let want = (fraction * total as f64).ceil() as u64;
    let mut seen = 0u64;
    for (bin, &count) in histogram.iter().enumerate() {
        seen += u64::from(count);
        if seen >= want {
            return (bin as f32 + 0.5) / 255.0;
        }
    }
    1.0
}

/// Pick the curve to encode: the user's numbers, or ones derived per picture.
fn resolve_tone(primary: &RawImage, settings: &Settings) -> ToneParams {
    if settings.auto_tone {
        return auto_tone(primary);
    }
    let max_boost = settings.max_boost.clamp(1.0, 16.0);
    ToneParams {
        knee: settings.knee.clamp(0.0, 0.999),
        max_boost,
        // A manual curve is described by its own bounds, not by the picture.
        top_level: 1.0,
        top_stops: max_boost.log2(),
    }
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
    let tone = resolve_tone(&primary, settings);
    let (gain_image, gain_metadata) = build_gain_map_with(&primary, settings.scale, tone);
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
    let tone = resolve_tone(primary, settings);
    build_gain_map_with(primary, settings.scale, tone)
}

/// [`build_gain_map`] with the curve given explicitly, for callers that already
/// resolved it (or measured it, as the automatic path does).
pub fn build_gain_map_with(
    primary: &RawImage,
    scale: u32,
    tone: ToneParams,
) -> (RawImage, GainMapMetadata) {
    let width = primary.width;
    let height = primary.height;
    let scale = scale.max(1);
    let gain_width = width.div_ceil(scale);
    let gain_height = height.div_ceil(scale);

    let max_boost = tone.max_boost.clamp(1.0, 16.0);
    let log_max = max_boost.ln();
    let knee = tone.knee.clamp(0.0, 0.999);
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
