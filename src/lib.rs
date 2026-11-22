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

pub mod mask;

// The HEIC writer pulls in a pure-Rust HEVC encoder, which is a desktop-only
// dependency: on arm64 Android it would add roughly 9x the encode time of the
// JPEG path for a format the share sheet barely uses. `Settings::format` still
// exists there so the struct is shared, and `convert` reports a clear error
// instead of silently falling back.
#[cfg(not(target_os = "android"))]
pub mod heic;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
pub use mask::{BrushMask, BrushStroke, MaskCombine, MaskLayer, MaskShape, MaskStack, rgb_to_hsl};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Ultra HDR JPEG: SDR primary + gain map in an MPF container. Universally
    /// readable, including by the SDR fallback of every viewer.
    #[default]
    Jpeg,
    /// ISO 21496-1 HEIC: HEVC-coded primary + gain map plane. Smaller at equal
    /// quality, but roughly nine times slower to encode and unreadable by
    /// software that predates HEIF.
    Heic,
}

impl OutputFormat {
    /// File extension for [`output_path`], without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Heic => "heic",
        }
    }

    /// Same as [`Self::extension`] with the leading dot, for UI text.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Jpeg => ".jpg",
            Self::Heic => ".heic",
        }
    }

    /// Stable token for the on-disk settings file.
    pub fn code(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Heic => "heic",
        }
    }

    pub fn from_code(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => Some(Self::Jpeg),
            "heic" | "heif" => Some(Self::Heic),
            _ => None,
        }
    }
}

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
    /// Local adjustment layers applied on top of the global curve. Empty = off.
    ///
    /// `Arc` because every batch worker clones the whole [`Settings`], and a
    /// long editing session would otherwise be copied per file.
    pub masks: Arc<MaskStack>,
    /// Container written by [`convert`]. Both formats carry the same gain map;
    /// they differ in codec and in what software can open the result.
    pub format: OutputFormat,
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
            masks: Arc::new(MaskStack::default()),
            format: OutputFormat::Jpeg,
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
    tone_from_percentiles(p95, p999)
}

/// The part of [`auto_tone`] that only needs the two percentiles.
///
/// Split out so the editor can run the same rule on its low-resolution copy:
/// a 1000-pixel-wide thumbnail and the full frame agree on where the shoulder
/// belongs to within the resolution of the slider.
pub fn tone_from_percentiles(p95: f32, p999: f32) -> ToneParams {
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
pub fn resolve_tone(primary: &RawImage, settings: &Settings) -> ToneParams {
    let mut tone = if settings.auto_tone {
        auto_tone(primary)
    } else {
        let max_boost = settings.max_boost.clamp(1.0, 16.0);
        ToneParams {
            knee: settings.knee.clamp(0.0, 0.999),
            max_boost,
            // A manual curve is described by its own bounds, not by the picture.
            top_level: 1.0,
            top_stops: max_boost.log2(),
        }
    };

    // Local layers can only reach the ceiling the metadata advertises. A flat,
    // dim picture makes the automatic pass hand back an almost empty curve (see
    // the early return in `auto_tone`), which would cap a deliberate mask at
    // 1.5x. Raise the ceiling when a mask actually asks for more; pixels outside
    // the masks keep their own gain, so this does not lift the rest of the frame.
    let wanted = settings.masks.capacity(tone);
    if wanted > tone.max_boost {
        tone.max_boost = wanted;
        tone.top_stops = tone.max_boost.log2();
    }

    tone
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

/// Decode a JPEG into a downscaled RGBA buffer, for showing it on screen.
///
/// A 20 MP frame is 60 MB of RGB; shrinking by an integer step is both cheaper
/// and good enough for a preview, so no filtering is done. Returns
/// `(width, height, rgba8)`.
pub fn thumbnail_rgba(path: &Path, max_edge: u32) -> Result<(u32, u32, Vec<u8>)> {
    let data = std::fs::read(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&data));
    decoder
        .decode_headers()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| "读取 JPEG 头信息失败")?;
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
        bail!("解码结果不完整：期望 {expected} 字节，实际 {}", pixels.len());
    }

    // Round the step *up* so the result never exceeds `max_edge`; a plain
    // division would let 5568px / 5 = 1114px through a 1080px limit.
    let step = (width.max(height)).div_ceil(max_edge.max(1)).max(1);
    let stride = width as usize;
    let out_w = width.div_ceil(step);
    let out_h = height.div_ceil(step);
    let mut rgba = Vec::with_capacity((out_w as usize) * (out_h as usize) * 4);
    for y in (0..height).step_by(step as usize) {
        for x in (0..width).step_by(step as usize) {
            let index = (y as usize * stride + x as usize) * 3;
            rgba.extend_from_slice(&[
                pixels[index],
                pixels[index + 1],
                pixels[index + 2],
                255,
            ]);
        }
    }

    Ok((out_w, out_h, rgba))
}

/// A small copy of the picture the editor can evaluate masks against.
///
/// Masks are stored in normalized coordinates, so the same weights that decide
/// the real gain map can be computed on a 1000-pixel-wide copy and drawn
/// instantly. That is what keeps dragging a gradient cheap: only this buffer is
/// touched until the user lets go and the full-resolution conversion runs.
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// RGB8, `width * height * 3` bytes.
    pub rgb: Arc<Vec<u8>>,
    /// Perceptual luminance per pixel, precomputed once.
    level: Vec<f32>,
}

impl PreviewFrame {
    pub fn load(path: &Path, max_edge: u32) -> Result<Self> {
        let (width, height, rgba) = thumbnail_rgba(path, max_edge)?;
        let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
        let mut level = Vec::with_capacity((width as usize) * (height as usize));
        let luma = luma_coefficients(ColorGamut::Bt709);
        for pixel in rgba.chunks_exact(4) {
            let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
            rgb.extend_from_slice(&[r, g, b]);
            let luminance = luma[0] * srgb_eotf(r as f32 / 255.0)
                + luma[1] * srgb_eotf(g as f32 / 255.0)
                + luma[2] * srgb_eotf(b as f32 / 255.0);
            level.push(srgb_oetf(luminance.clamp(0.0, 1.0)));
        }
        Ok(Self {
            width,
            height,
            rgb: Arc::new(rgb),
            level,
        })
    }

    /// Perceptual luminance at one pixel, `0..1`.
    ///
    /// The editor uses this to seed the luminance tool from whatever the pointer
    /// is over, and to evaluate a mask while a drag is still in progress.
    pub fn level_at(&self, x: usize, y: usize) -> f32 {
        let x = x.min(self.width.saturating_sub(1) as usize);
        let y = y.min(self.height.saturating_sub(1) as usize);
        self.level[y * self.width as usize + x]
    }

    /// The same two percentiles [`auto_tone`] uses, measured on this copy.
    ///
    /// The editor must show the curve the export will use, but it has no
    /// full-resolution pixels, so it runs the rule on its own histogram. A 1000
    /// px copy agrees with the original to within a slider step.
    pub fn tone(&self) -> ToneParams {
        let mut histogram = [0u32; 256];
        for &level in &self.level {
            let bin = (level * 255.0).round().clamp(0.0, 255.0) as usize;
            histogram[bin] += 1;
        }
        let count: u64 = histogram.iter().map(|&c| u64::from(c)).sum();
        if count == 0 {
            return tone_from_percentiles(1.0, 1.0);
        }
        tone_from_percentiles(
            percentile(&histogram, count, 0.95),
            percentile(&histogram, count, 0.999),
        )
    }

    /// Combined mask weight per pixel, `0..1`.
    pub fn coverage(&self, masks: &MaskStack) -> Vec<f32> {
        let count = (self.width as usize) * (self.height as usize);
        let mut out = vec![0.0f32; count];
        // `has_weight`, not `has_effect`: a new mask follows the base curve and
        // changes no gain, but the overlay still has to show where it is.
        if !masks.has_weight() {
            return out;
        }
        let prepared = masks.prepare(self.width, self.height);
        let (fw, fh) = (self.width as f32, self.height as f32);
        out.par_chunks_mut(self.width as usize)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, slot) in row.iter_mut().enumerate() {
                    let index = y * self.width as usize + x;
                    let rgb = [
                        self.rgb[index * 3],
                        self.rgb[index * 3 + 1],
                        self.rgb[index * 3 + 2],
                    ];
                    *slot = masks.weight_at(
                        &prepared,
                        index,
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                        fw,
                        fh,
                        self.level[index],
                        rgb,
                    );
                }
            });
        out
    }

    /// Paint one layer's weight on top of the picture, for the list's
    /// "show only this mask" toggle.
    pub fn overlay_one_rgba(&self, masks: &MaskStack, layer: usize) -> Vec<u8> {
        let count = (self.width as usize) * (self.height as usize);
        let buffer = masks.prepare_one(layer, self.width, self.height);
        let (fw, fh) = (self.width as f32, self.height as f32);
        let mut coverage = vec![0.0f32; count];
        coverage
            .par_chunks_mut(self.width as usize)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, slot) in row.iter_mut().enumerate() {
                    let index = y * self.width as usize + x;
                    let rgb = [
                        self.rgb[index * 3],
                        self.rgb[index * 3 + 1],
                        self.rgb[index * 3 + 2],
                    ];
                    *slot = masks.weight_of_layer(
                        layer,
                        buffer.as_deref(),
                        index,
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                        fw,
                        fh,
                        self.level[index],
                        rgb,
                    );
                }
            });
        self.tint(&coverage)
    }

    /// Paint the mask weights on top of the picture: amber where a layer adds
    /// brightness, red where it subtracts.
    pub fn overlay_rgba(&self, masks: &MaskStack) -> Vec<u8> {
        self.tint(&self.coverage(masks))
    }

    fn tint(&self, coverage: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(coverage.len() * 4);
        for (index, weight) in coverage.iter().enumerate() {
            let (r, g, b) = (
                self.rgb[index * 3],
                self.rgb[index * 3 + 1],
                self.rgb[index * 3 + 2],
            );
            let mix = (weight.clamp(0.0, 1.0) * 0.65).min(1.0);
            let tint = [255.0f32, 150.0, 40.0];
            out.extend_from_slice(&[
                (r as f32 * (1.0 - mix) + tint[0] * mix) as u8,
                (g as f32 * (1.0 - mix) + tint[1] * mix) as u8,
                (b as f32 * (1.0 - mix) + tint[2] * mix) as u8,
                255,
            ]);
        }
        out
    }

    /// Simulate what an HDR viewer would show, so the mask's effect on the
    /// picture is visible before exporting anything.
    ///
    /// The gain map is encoded in log space; this decodes the same value and
    /// applies it to the SDR pixels, then tone-maps back into `0..255` the way a
    /// display would.
    pub fn simulated_rgba(&self, masks: &MaskStack, tone: ToneParams, exposure: f32) -> Vec<u8> {
        let count = (self.width as usize) * (self.height as usize);
        let prepared = if masks.has_effect() {
            masks.prepare(self.width, self.height)
        } else {
            Vec::new()
        };
        let active = masks.has_effect();
        let (fw, fh) = (self.width as f32, self.height as f32);
        let knee = tone.knee.clamp(0.0, 0.999);
        let span = (1.0 - knee).max(1e-6);
        let max_boost = tone.max_boost.clamp(1.0, 16.0);
        let mut out = vec![0u8; count * 4];
        let exposure = exposure.clamp(0.05, 8.0);

        out.par_chunks_mut(self.width as usize * 4)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let index = y * self.width as usize + x;
                    let rgb = [
                        self.rgb[index * 3],
                        self.rgb[index * 3 + 1],
                        self.rgb[index * 3 + 2],
                    ];
                    let level = self.level[index];
                    let t = ((level - knee) / span).clamp(0.0, 1.0);
                    let t = t * t * (3.0 - 2.0 * t);
                    let base_gain = 1.0 + (max_boost - 1.0) * t;
                    let gain = if active {
                        masks.gain_at(
                            &prepared,
                            index,
                            x as f32 + 0.5,
                            y as f32 + 0.5,
                            fw,
                            fh,
                            level,
                            rgb,
                            base_gain,
                            tone,
                        )
                    } else {
                        base_gain
                    };
                    for channel in 0..3 {
                        let linear = srgb_eotf(rgb[channel] as f32 / 255.0) * gain * exposure;
                        // Clamp in linear light before re-encoding: that is the
                        // clip a real display imposes, and it keeps highlights
                        // from banding as they compress.
                        let encoded = srgb_oetf(linear.clamp(0.0, 1.0));
                        pixel[channel] = (encoded * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                    pixel[3] = 255;
                }
            });
        out
    }
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
    let (gain_image, gain_metadata) =
        build_gain_map_with(&primary, settings.scale, tone, &settings.masks);
    let (gain_width, gain_height) = (gain_image.width, gain_image.height);
    let peak_boost = gain_metadata.max_content_boost[0];

    let output = match settings.format {
        OutputFormat::Jpeg => encode_jpeg(
            &primary,
            gain_image,
            gain_metadata,
            color,
            exif,
            settings,
        )?,
        OutputFormat::Heic => {
            // The HEIC writer takes the ICC separately, so pull it out before
            // `color` would be consumed by the JPEG options.
            let icc = color.icc_profile.clone();
            encode_heic(
                &primary,
                &gain_image,
                &gain_metadata,
                exif.as_deref(),
                icc.as_deref(),
                settings,
            )?
        }
    };

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

/// Write the Ultra HDR JPEG: SDR primary + gain map in one MPF container.
fn encode_jpeg(
    primary: &RawImage,
    gain_image: RawImage,
    gain_metadata: GainMapMetadata,
    color: ColorMetadata,
    exif: Option<Vec<u8>>,
    settings: &Settings,
) -> Result<Vec<u8>> {
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

    let output = ultrajpeg::encode(primary, &options).with_context(|| "编码 Ultra HDR 失败")?;

    // Cheap sanity check: the container must advertise a gain map.
    let check = ultrajpeg::inspect(&output).with_context(|| "回读校验失败")?;
    if check.gain_map_jpeg_len.is_none() {
        bail!("输出文件缺少增益图");
    }
    Ok(output)
}

/// Write the ISO 21496-1 HEIC, reusing the gain map the JPEG path would embed.
#[cfg(not(target_os = "android"))]
fn encode_heic(
    primary: &RawImage,
    gain_image: &RawImage,
    gain_metadata: &GainMapMetadata,
    exif: Option<&[u8]>,
    icc: Option<&[u8]>,
    settings: &Settings,
) -> Result<Vec<u8>> {
    heic::encode(
        primary,
        gain_image,
        gain_metadata,
        settings.primary_quality,
        exif,
        icc,
    )
    .with_context(|| "编码 HEIC 失败")
}

/// Android builds ship without the HEVC encoder; say so instead of pretending.
#[cfg(target_os = "android")]
fn encode_heic(
    _primary: &RawImage,
    _gain_image: &RawImage,
    _gain_metadata: &GainMapMetadata,
    _exif: Option<&[u8]>,
    _icc: Option<&[u8]>,
    _settings: &Settings,
) -> Result<Vec<u8>> {
    bail!("此版本不支持 HEIC 输出，请改用 JPG")
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
    build_gain_map_with(primary, settings.scale, tone, &settings.masks)
}

/// [`build_gain_map`] with the curve given explicitly, for callers that already
/// resolved it (or measured it, as the automatic path does).
///
/// `masks` is folded on top of the global curve in **gain space**: every layer
/// lerps the running gain toward the gain its own curve would produce, so the
/// result stays between the base and the layer's ceiling instead of adding
/// stops. Where no mask reaches, the global value survives byte for byte.
pub fn build_gain_map_with(
    primary: &RawImage,
    scale: u32,
    tone: ToneParams,
    masks: &MaskStack,
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

    // Masks are evaluated at gain-map resolution: the buffer is at most a
    // quarter of the picture, and `scale` already averages it.
    let mask_active = masks.has_effect();
    let prepared = if mask_active {
        masks.prepare(gain_width, gain_height)
    } else {
        Vec::new()
    };
    let (fw, fh) = (gain_width as f32, gain_height as f32);

    let mut data = vec![0u8; (gain_width as usize) * (gain_height as usize)];

    data.par_chunks_mut(gain_width as usize)
        .enumerate()
        .for_each(|(gy, row)| {
            let sy = ((gy as u32) * scale + scale / 2).min(height - 1);
            let base = sy * primary.stride;
            let row_start = (gy as usize) * (gain_width as usize);
            for (gx, slot) in row.iter_mut().enumerate() {
                let sx = ((gx as u32) * scale + scale / 2).min(width - 1);
                let idx = (base + sx * 3) as usize;
                let rgb = [
                    primary.data[idx],
                    primary.data[idx + 1],
                    primary.data[idx + 2],
                ];
                let luminance = luma[0] * srgb_eotf(rgb[0] as f32 / 255.0)
                    + luma[1] * srgb_eotf(rgb[1] as f32 / 255.0)
                    + luma[2] * srgb_eotf(rgb[2] as f32 / 255.0);

                let level = srgb_oetf(luminance.clamp(0.0, 1.0));
                let t = ((level - knee) / span).clamp(0.0, 1.0);
                let t = t * t * (3.0 - 2.0 * t); // smoothstep
                let base_gain = 1.0 + (max_boost - 1.0) * t;

                let gain = if mask_active {
                    masks.gain_at(
                        &prepared,
                        row_start + gx,
                        gx as f32 + 0.5,
                        gy as f32 + 0.5,
                        fw,
                        fh,
                        level,
                        rgb,
                        base_gain,
                        tone,
                    )
                } else {
                    base_gain
                };

                *slot = if log_max > 1e-6 {
                    ((gain.clamp(1.0, max_boost).ln() / log_max) * 255.0)
                        .round()
                        .clamp(0.0, 255.0) as u8
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

/// `photo.jpg` -> `photo_hdr.jpg` (or `photo_hdr.heic`), either beside the
/// source or in `out_dir`.
///
/// The extension follows the requested [`OutputFormat`]; writing HEVC bytes into
/// a `.jpg` name would make half the viewers refuse it.
pub fn output_path(src: &Path, out_dir: Option<&Path>, format: OutputFormat) -> std::path::PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_owned());
    let name = format!("{stem}_hdr.{}", format.extension());
    match out_dir {
        Some(dir) => dir.join(name),
        None => src.with_file_name(name),
    }
}
