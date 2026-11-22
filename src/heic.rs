//! HEIC output: pack an already-computed gain map into an ISO 21496-1 HEIC.
//!
//! The JPEG path hands `ultrajpeg` the gain map as pixels. HEIC has no such
//! entry point: `heifer` takes a *base image plus an HDR image* and derives the
//! gain map from their ratio. That looks like a loss of control, but it is not —
//! the derivation is exactly invertible.
//!
//! `heifer` reads a gain map sample as
//!
//! ```text
//! ratio  = max(hdr_r / base_r, hdr_g / base_g, hdr_b / base_b)
//! sample = log2(ratio) / log2(headroom)
//! ```
//!
//! so feeding it `hdr = linear(base) * gain` makes every channel ratio equal to
//! `gain`, the max is `gain`, and the sample it stores is
//! `log2(gain) / log2(max_boost)` — the same value
//! [`crate::build_gain_map_with`] writes. The only difference left is HEVC
//! quantisation of the gain map plane itself, measured at about 1.3% mean
//! relative error on a 20 MP frame.
//!
//! Two limits of the crate are worth knowing:
//!
//! * the gain map is always written at **half resolution** (`div_ceil(2)` of the
//!   base), so a `scale` of 1 is downsampled and 4 is upsampled. The mask
//!   geometry is preserved, edges soften slightly;
//! * there is no metadata path for an arbitrary gain map, so `max_boost` must
//!   come from the gain map we built — it does.

use anyhow::{Result, anyhow};
use ultrahdr_core::color::srgb_eotf;
use ultrahdr_core::{GainMapMetadata, RawImage};

/// Encode `primary` + `gain` as a single HEIC file.
///
/// `quality` is the HEVC quality (1..=100). `exif` and `icc` are copied into the
/// container as-is; the pixels are untouched, so an EXIF orientation still
/// applies to them.
pub fn encode(
    primary: &RawImage,
    gain: &RawImage,
    metadata: &GainMapMetadata,
    quality: u8,
    exif: Option<&[u8]>,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let (width, height) = (primary.width, primary.height);
    if width == 0 || height == 0 {
        return Err(anyhow!("图像尺寸为空"));
    }

    // `heifer` rejects a headroom of exactly 1, and a gain map that never boosts
    // anything carries no HDR information — refuse rather than write a file that
    // claims to be HDR and is not.
    let max_boost = metadata.max_content_boost[0];
    if !(max_boost > 1.000_001) {
        return Err(anyhow!(
            "提亮上限为 {max_boost:.3}×，没有可写入的 HDR 信息；HEIC 至少需要大于 1.0"
        ));
    }
    let log_max = max_boost.ln();

    // The gain map is `scale` times smaller than the primary. Sampling it the
    // same way `build_gain_map_with` does (nearest block) keeps the two in step;
    // heifer will downsample to half resolution afterwards regardless.
    let scale_x = width.div_ceil(gain.width.max(1)).max(1) as usize;
    let scale_y = height.div_ceil(gain.height.max(1)).max(1) as usize;
    let gain_stride = gain.stride as usize;
    let gain_width = gain.width as usize;
    let primary_stride = primary.stride as usize;

    let count = (width as usize) * (height as usize);
    let mut base = heifer::Image {
        width,
        height,
        bit_depth: 8,
        has_alpha: false,
        data: Vec::with_capacity(count * 4),
    };
    let mut hdr = heifer::HdrImage {
        width,
        height,
        headroom: max_boost,
        data: Vec::with_capacity(count * 3),
    };

    for y in 0..height as usize {
        let gain_row = (y / scale_y).min(gain.height as usize - 1) * gain_stride;
        let primary_row = y * primary_stride;
        for x in 0..width as usize {
            let index = primary_row + x * 3;
            let gain_x = (x / scale_x).min(gain_width - 1);
            let byte = gain.data[gain_row + gain_x];

            // Byte -> gain, the inverse of the encoder in `build_gain_map_with`.
            let factor = ((byte as f32 / 255.0) * log_max).exp();

            let r = primary.data[index];
            let g = primary.data[index + 1];
            let b = primary.data[index + 2];
            base.data.extend_from_slice(&[r as u16, g as u16, b as u16, u16::MAX]);

            hdr.data.push(srgb_eotf(r as f32 / 255.0) * factor);
            hdr.data.push(srgb_eotf(g as f32 / 255.0) * factor);
            hdr.data.push(srgb_eotf(b as f32 / 255.0) * factor);
        }
    }

    let metadata = heifer::Metadata {
        exif: exif.map(<[u8]>::to_vec),
        xmp: None,
        icc_profile: icc.map(<[u8]>::to_vec),
    };
    let options = heifer::EncodeOptions::new().with_quality(quality.clamp(1, 100));

    heifer::encode_hdr_with_metadata(&base, &hdr, &metadata, &options)
        .map_err(|e| anyhow!("HEIC 编码失败：{e:?}"))
}
