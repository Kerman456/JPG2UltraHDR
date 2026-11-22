//! Local adjustment masks — a Lightroom-style layer stack.
//!
//! The global tone curve decides one knee and one ceiling for the whole frame.
//! Real scenes are not uniform: a window in a dim room, a lamp, a patch of sky.
//! A *mask* answers "where", and carries its own curve, so the same picture can
//! be lifted in one region and left alone in another.
//!
//! # Composition
//!
//! Every layer rasterizes to a weight in `0..1`, then folds into one gain
//! multiplier per pixel. The fold is a lerp in **gain space** (not in stops and
//! not in the encoded byte), which is the formula the UI describes:
//!
//! ```text
//! gain = base_gain
//! for each enabled layer:
//!     w = mask_weight             (shape × invert × opacity)
//!     g = layer curve at this pixel's luminance
//!     Add        gain = gain + w * (g - gain)      // pull toward the layer
//!     Subtract   gain = gain + w * (1 - gain)      // pull back to untouched
//!     Intersect  same as Add, with w *= coverage_so_far
//!     coverage = coverage + w * (1 - coverage)     (or *= 1 - w when subtracting)
//! ```
//!
//! Lerping toward the layer's own gain (rather than adding stops) keeps every
//! intermediate value bounded by `max(base, layer)` — so the gain map can never
//! overflow the ceiling the metadata advertises, no matter how layers stack.
//! Subtracting pulls toward `1.0`, i.e. removes brightness without clipping to
//! black, which is what "protect this face from the global lift" means.
//!
//! # Coordinates
//!
//! Geometry is stored in normalized `0..1` coordinates so one mask applies to
//! every picture in a batch and survives a change of gain-map resolution. The
//! brush radius is normalized against the image **width** in both axes, which
//! is what keeps the brush round whatever the aspect ratio.

use rayon::prelude::*;

use crate::ToneParams;

/// Hard caps so a runaway session cannot make the config file unbounded.
pub const MAX_LAYERS: usize = 64;
pub const MAX_STROKES: usize = 400;
pub const MAX_POINTS_PER_STROKE: usize = 4000;

/// How a layer combines with everything below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskCombine {
    /// Union: the layer's curve takes over where the mask is.
    Add,
    /// Difference: the layer removes brightness where the mask is.
    Subtract,
    /// Intersection: the layer only reaches pixels already covered below it.
    Intersect,
}

impl MaskCombine {
    pub fn label(self) -> &'static str {
        match self {
            Self::Add => "叠加",
            Self::Subtract => "减去",
            Self::Intersect => "交集",
        }
    }

    pub fn code(self) -> char {
        match self {
            Self::Add => 'a',
            Self::Subtract => 's',
            Self::Intersect => 'i',
        }
    }

    pub fn from_code(code: char) -> Self {
        match code {
            's' => Self::Subtract,
            'i' => Self::Intersect,
            _ => Self::Add,
        }
    }
}

// ---- brush ----------------------------------------------------------------

/// One painted (or erased) polyline.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushStroke {
    /// `true` = eraser: removes coverage instead of adding it.
    pub erase: bool,
    /// Brush radius in units of the image width.
    pub radius: f32,
    /// `1.0` = hard edge, `0.0` = fades from the centre out.
    pub hardness: f32,
    /// Per-stroke opacity; the layer has a master opacity on top.
    pub flow: f32,
    /// Polyline in normalized coordinates.
    pub points: Vec<[f32; 2]>,
}

impl BrushStroke {
    pub fn new(radius: f32, hardness: f32, flow: f32, erase: bool, start: [f32; 2]) -> Self {
        Self {
            erase,
            radius: radius.clamp(0.0005, 0.5),
            hardness: hardness.clamp(0.0, 1.0),
            flow: flow.clamp(0.0, 1.0),
            points: vec![start],
        }
    }

    /// Append a sample, dropping samples closer than a fraction of the brush:
    /// denser points add nothing and would bloat the config file.
    pub fn push(&mut self, point: [f32; 2]) {
        let last = self.points.last().copied().unwrap_or(point);
        let dx = point[0] - last[0];
        let dy = point[1] - last[1];
        if (dx * dx + dy * dy).sqrt() < (self.radius * 0.35).max(0.002) {
            return;
        }
        if self.points.len() < MAX_POINTS_PER_STROKE {
            self.points.push(point.clamp_unit());
        }
    }
}

trait ClampUnit {
    fn clamp_unit(self) -> Self;
}

impl ClampUnit for [f32; 2] {
    fn clamp_unit(self) -> Self {
        [self[0].clamp(0.0, 1.0), self[1].clamp(0.0, 1.0)]
    }
}

/// The stroke stack behind a brush mask.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrushMask {
    pub strokes: Vec<BrushStroke>,
}

impl BrushMask {
    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty()
    }

    pub fn push(&mut self, stroke: BrushStroke) {
        if stroke.points.is_empty() || self.strokes.len() >= MAX_STROKES {
            return;
        }
        self.strokes.push(stroke);
    }

    pub fn undo(&mut self) -> bool {
        self.strokes.pop().is_some()
    }

    pub fn clear(&mut self) {
        self.strokes.clear();
    }

    /// Rasterize coverage at mask resolution.
    ///
    /// Coverage composes as an ordinary "over" operation, so the result does
    /// not depend on how many times a pixel was touched:
    ///
    /// ```text
    /// paint: cover += a * (1 - cover)
    /// erase: cover *= 1 - a
    /// ```
    pub fn rasterize(&self, width: u32, height: u32) -> Vec<f32> {
        let stride = width as usize;
        let mut out = vec![0.0f32; stride * height as usize];
        if self.strokes.is_empty() || width == 0 || height == 0 {
            return out;
        }

        let w = width as f32;
        let h = height as f32;

        for stroke in &self.strokes {
            if stroke.points.is_empty() {
                continue;
            }
            // Geometry in mask pixels. `radius` is in width units, so x and y
            // share it — a round brush.
            let radius = (stroke.radius * w).max(0.75);
            let inner = radius * stroke.hardness;
            // Width of the soft rim. Even at hardness 1 a sub-pixel rim is kept
            // so the edge is antialiased instead of staircased.
            let band = (radius - inner).max(0.75);
            let flow = stroke.flow;

            // Bounding box of the whole polyline, so a small stroke does not
            // scan the entire mask.
            let (mut min_u, mut min_v) = (f32::MAX, f32::MAX);
            let (mut max_u, mut max_v) = (f32::MIN, f32::MIN);
            for point in &stroke.points {
                min_u = min_u.min(point[0]);
                min_v = min_v.min(point[1]);
                max_u = max_u.max(point[0]);
                max_v = max_v.max(point[1]);
            }
            let margin = radius + band;
            let x0 = ((min_u * w) - margin).floor().max(0.0) as u32;
            let y0 = ((min_v * h) - margin).floor().max(0.0) as u32;
            let x1 = (((max_u * w) + margin).ceil() as u32).min(width.saturating_sub(1));
            let y1 = (((max_v * h) + margin).ceil() as u32).min(height.saturating_sub(1));
            if x1 < x0 || y1 < y0 {
                continue;
            }

            let points: Vec<(f32, f32)> = stroke
                .points
                .iter()
                .map(|p| (p[0] * w, p[1] * h))
                .collect();

            let rows = (y1 - y0 + 1) as usize;
            // Skip to the first row inside the bounding box, otherwise the
            // stroke lands near the top of the mask instead of where it was
            // drawn.
            let skip = (y0 as usize) * stride;
            let cover = &mut out[skip..];

            let mut slots: Vec<&mut [f32]> = Vec::with_capacity(rows);
            {
                let mut rest: &mut [f32] = cover;
                for _ in 0..rows {
                    let (head, tail) = rest.split_at_mut(stride);
                    slots.push(head);
                    rest = tail;
                }
            }

            let erase = stroke.erase;
            slots
                .into_par_iter()
                .enumerate()
                .for_each(|(row, cover_row)| {
                    let y = (y0 + row as u32) as f32 + 0.5;
                    for x in x0..=x1 {
                        let px = x as f32 + 0.5;
                        let distance = distance_to_polyline(&points, px, y);
                        if distance >= radius {
                            continue;
                        }
                        let a = (((radius - distance) / band).clamp(0.0, 1.0) * flow).clamp(0.0, 1.0);
                        if a <= 0.0 {
                            continue;
                        }
                        let index = x as usize;
                        if erase {
                            cover_row[index] *= 1.0 - a;
                        } else {
                            cover_row[index] += a * (1.0 - cover_row[index]);
                        }
                    }
                });
        }

        out
    }
}

/// Shortest distance from `(px, py)` to the polyline, in pixels.
fn distance_to_polyline(points: &[(f32, f32)], px: f32, py: f32) -> f32 {
    if points.len() == 1 {
        let (x, y) = points[0];
        return ((px - x).powi(2) + (py - y).powi(2)).sqrt();
    }
    let mut best = f32::MAX;
    for pair in points.windows(2) {
        let (ax, ay) = pair[0];
        let (bx, by) = pair[1];
        let (dx, dy) = (bx - ax, by - ay);
        let length_squared = dx * dx + dy * dy;
        let t = if length_squared > 1e-9 {
            (((px - ax) * dx + (py - ay) * dy) / length_squared).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cy) = (ax + dx * t, ay + dy * t);
        best = best.min(((px - cx).powi(2) + (py - cy).powi(2)).sqrt());
    }
    best
}

// ---- shapes ---------------------------------------------------------------

/// What the mask looks at.
#[derive(Debug, Clone, PartialEq)]
pub enum MaskShape {
    /// Drifting from `start` (weight 0) to `end` (weight 1).
    Linear {
        start: [f32; 2],
        end: [f32; 2],
        /// Pushes the fade-out point back past `start`, as a fraction of the
        /// `start`→`end` distance. 0 = the drag itself is the whole ramp.
        feather: f32,
    },
    /// An ellipse: full weight inside, fading to zero at the rim.
    Radial {
        center: [f32; 2],
        /// Radii as fractions of the image width and height.
        radius: [f32; 2],
        /// Clockwise rotation in radians.
        rotation: f32,
        /// Width of the fade band inside the rim, `0..1` of the radius.
        feather: f32,
    },
    /// Painted strokes.
    Brush(BrushMask),
    /// A band of the picture's own perceptual luminance.
    Luminance {
        min: f32,
        max: f32,
        /// Softness of both shoulders, `0..1`.
        softness: f32,
    },
    /// A band of the picture's own hue.
    Color {
        /// Target hue in `0..1` (0 = red, 1/3 = green, 2/3 = blue).
        hue: f32,
        /// Half-width of the accepted hue band, `0..0.5`.
        hue_tolerance: f32,
        /// Minimum saturation, `0..1`; grey pixels fall out of the mask.
        saturation_min: f32,
        /// Extra softening of the hue band, `0..1`.
        softness: f32,
    },
}

impl MaskShape {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Linear { .. } => "线性渐变",
            Self::Radial { .. } => "径向渐变",
            Self::Brush(_) => "画笔",
            Self::Luminance { .. } => "亮度范围",
            Self::Color { .. } => "颜色范围",
        }
    }

    /// The "feather" knob the panel exposes, normalized to `0..1` where that is
    /// meaningful. Luminance and colour use their own softness.
    pub fn feather(&self) -> f32 {
        match self {
            Self::Linear { feather, .. } => feather.clamp(0.0, 4.0) / 4.0,
            Self::Radial { feather, .. } => feather.clamp(0.0, 0.95),
            Self::Brush(strokes) => strokes
                .strokes
                .last()
                .map(|s| 1.0 - s.hardness)
                .unwrap_or(0.5),
            Self::Luminance { softness, .. } | Self::Color { softness, .. } => {
                softness.clamp(0.0, 1.0)
            }
        }
    }

    pub fn set_feather(&mut self, value: f32) {
        let value = value.clamp(0.0, 1.0);
        match self {
            Self::Linear { feather, .. } => *feather = value * 4.0,
            Self::Radial { feather, .. } => *feather = value.clamp(0.01, 0.95),
            Self::Brush(strokes) => {
                if let Some(last) = strokes.strokes.last_mut() {
                    last.hardness = 1.0 - value;
                }
            }
            Self::Luminance { softness, .. } | Self::Color { softness, .. } => *softness = value,
        }
    }

    /// Colour-dependent shapes must be evaluated against the source pixel;
    /// everything else can be baked into a buffer once per conversion.
    pub fn needs_source_pixels(&self) -> bool {
        matches!(
            self,
            Self::Luminance { .. } | Self::Color { .. } | Self::Brush(_)
        )
    }

    /// Weight at one mask pixel.
    ///
    /// `brush` is the precomputed buffer for brush shapes, `index` the offset of
    /// this pixel inside it. `level` is the pixel's perceptual luminance and
    /// `rgb` its source colour.
    #[inline]
    pub fn weight_at(
        &self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        level: f32,
        rgb: [u8; 3],
        brush: Option<&[f32]>,
        index: usize,
    ) -> f32 {
        match self {
            Self::Linear {
                start,
                end,
                feather,
            } => {
                let (ax, ay) = (start[0] * width, start[1] * height);
                let (bx, by) = (end[0] * width, end[1] * height);
                let (dx, dy) = (bx - ax, by - ay);
                let length_squared = dx * dx + dy * dy;
                if length_squared < 1e-6 {
                    return 0.0;
                }
                // Projection along the axis, 0 at `start`, 1 at `end`.
                let s = ((x - ax) * dx + (y - ay) * dy) / length_squared;
                // Feather moves the 0% line backwards past `start` while `end`
                // stays at 100%, so a longer feather means a gentler ramp.
                let f = feather.clamp(0.0, 4.0);
                smoothstep(((s + f) / (1.0 + f)).clamp(0.0, 1.0))
            }
            Self::Radial {
                center,
                radius,
                rotation,
                feather,
            } => {
                let (cx, cy) = (center[0] * width, center[1] * height);
                let rx = (radius[0] * width).max(0.5);
                let ry = (radius[1] * height).max(0.5);
                let (dx, dy) = (x - cx, y - cy);
                let (sin, cos) = rotation.sin_cos();
                // Undo the rotation, then squash into the unit circle.
                let qx = (dx * cos + dy * sin) / rx;
                let qy = (-dx * sin + dy * cos) / ry;
                let distance = (qx * qx + qy * qy).sqrt();
                let f = feather.clamp(0.01, 0.95);
                let edge = 1.0 - f;
                let t = ((distance - edge) / (1.0 - edge)).clamp(0.0, 1.0);
                1.0 - smoothstep(t)
            }
            Self::Brush(_) => brush
                .and_then(|buffer| buffer.get(index).copied())
                .unwrap_or(0.0),
            Self::Luminance {
                min,
                max,
                softness,
            } => {
                let wedge = (softness.clamp(0.0, 1.0) * 0.5).max(0.005);
                let low = *min;
                let high = max.max(*min);
                // At the ends of the scale there is nothing outside to fade
                // into: a band that starts at 0 or ends at 1 must select its
                // edge fully, otherwise "select everything" would max out at
                // half strength.
                let up = if low <= 1e-4 {
                    1.0
                } else {
                    smoothstep(((level - (low - wedge)) / (2.0 * wedge)).clamp(0.0, 1.0))
                };
                let down = if high >= 1.0 - 1e-4 {
                    1.0
                } else {
                    smoothstep((((high + wedge) - level) / (2.0 * wedge)).clamp(0.0, 1.0))
                };
                up * down
            }
            Self::Color {
                hue,
                hue_tolerance,
                saturation_min,
                softness,
            } => {
                let (h, s, _) = rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
                let mut difference = (h - hue).abs();
                if difference > 0.5 {
                    difference = 1.0 - difference;
                }
                // Softness widens the accepted band instead of blurring it, so
                // the control stays monotone: more softness, more pixels.
                let tolerance = (hue_tolerance.clamp(0.001, 0.5)
                    * (1.0 + softness.clamp(0.0, 1.0) * 3.0))
                    .min(0.5);
                let hue_weight = 1.0 - smoothstep((difference / tolerance).clamp(0.0, 1.0));
                let floor = saturation_min.clamp(0.0, 0.99);
                let saturation_weight =
                    smoothstep(((s - floor) / (1.0 - floor).max(1e-3)).clamp(0.0, 1.0));
                hue_weight * saturation_weight
            }
        }
    }
}

// ---- layers ---------------------------------------------------------------

/// One mask in the stack: a shape plus the curve it wants locally.
#[derive(Debug, Clone, PartialEq)]
pub struct MaskLayer {
    pub name: String,
    pub enabled: bool,
    /// Flip the shape's weight (`1 - w`).
    pub invert: bool,
    /// Master opacity applied after inversion.
    pub opacity: f32,
    pub combine: MaskCombine,
    /// Local curve. Ignored while [`Self::follow_base`] is set.
    pub knee: f32,
    pub max_boost: f32,
    /// Start with the global curve so a freshly drawn mask changes nothing
    /// until the user tunes it — the behaviour a Lightroom user expects.
    ///
    /// [`Self::new`] defaults to `true` so a decoded layer that predates this
    /// field keeps behaving as a no-op. Both editors override it to `false`
    /// when they create a mask, because "draw a mask and export an unchanged
    /// photo" reads as a bug: see the note in `editor.rs` / `mask_ui.rs`.
    pub follow_base: bool,
    pub shape: MaskShape,
}

impl MaskLayer {
    pub fn new(name: impl Into<String>, shape: MaskShape) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            invert: false,
            opacity: 1.0,
            combine: MaskCombine::Add,
            knee: 0.55,
            max_boost: 3.0,
            follow_base: true,
            shape,
        }
    }

    /// The curve this layer actually uses, resolved against the global one.
    pub fn tone(&self, base: ToneParams) -> (f32, f32) {
        if self.follow_base {
            (base.knee, base.max_boost)
        } else {
            (self.knee.clamp(0.0, 0.999), self.max_boost.clamp(1.0, 16.0))
        }
    }
}

/// The whole stack, applied bottom to top.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaskStack {
    layers: Vec<MaskLayer>,
}

impl MaskStack {
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn layers(&self) -> &[MaskLayer] {
        &self.layers
    }

    pub fn layers_mut(&mut self) -> &mut Vec<MaskLayer> {
        &mut self.layers
    }

    pub fn push(&mut self, layer: MaskLayer) {
        if self.layers.len() < MAX_LAYERS {
            self.layers.push(layer);
        }
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index < self.layers.len() {
            self.layers.remove(index);
            true
        } else {
            false
        }
    }

    /// True when at least one enabled layer can change a pixel.
    ///
    /// A layer that still follows the base curve is a no-op, so a stack made
    /// only of untouched masks reports `false` and the fast path stays.
    pub fn has_effect(&self) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.enabled && layer.opacity > 0.0 && !layer.follow_base)
    }

    /// True when at least one enabled layer covers any pixel at all.
    ///
    /// This is the question the *overlay* asks, and it is deliberately looser
    /// than [`Self::has_effect`]: a freshly drawn mask follows the base curve,
    /// so it changes no gain, but the user still needs to see where it landed.
    /// Gating the overlay on `has_effect` made every new mask invisible until
    /// it was also given its own curve.
    pub fn has_weight(&self) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.enabled && layer.opacity > 0.0)
    }

    /// Highest gain any layer can ask for, which the metadata must advertise.
    pub fn capacity(&self, base: ToneParams) -> f32 {
        let mut cap = base.max_boost.clamp(1.0, 16.0);
        for layer in &self.layers {
            if !layer.enabled || layer.follow_base {
                continue;
            }
            cap = cap.max(layer.max_boost.clamp(1.0, 16.0));
        }
        cap
    }

    /// Bake the layers that do not need source pixels. The returned vector is
    /// aligned with [`Self::layers`]; `None` means "evaluate inline".
    pub fn prepare(&self, width: u32, height: u32) -> Vec<Option<Vec<f32>>> {
        self.layers
            .iter()
            .map(|layer| match &layer.shape {
                MaskShape::Brush(brush) if !brush.is_empty() => {
                    Some(brush.rasterize(width, height))
                }
                _ => None,
            })
            .collect()
    }

    /// Fold the stack into the final gain multiplier for one pixel.
    ///
    /// `base_gain` is what the global curve produced; the return value is
    /// always in `1.0..=capacity()`.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn gain_at(
        &self,
        prepared: &[Option<Vec<f32>>],
        index: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        level: f32,
        rgb: [u8; 3],
        base_gain: f32,
        base: ToneParams,
    ) -> f32 {
        let mut gain = base_gain;
        let mut coverage = 0.0f32;

        for (layer, buffer) in self.layers.iter().zip(prepared.iter()) {
            if !layer.enabled || layer.opacity <= 0.0 {
                continue;
            }
            let raw = layer.shape.weight_at(
                x,
                y,
                width,
                height,
                level,
                rgb,
                buffer.as_deref(),
                index,
            );
            let raw = if layer.invert { 1.0 - raw } else { raw };
            let weight = (raw * layer.opacity).clamp(0.0, 1.0);
            if weight <= 0.0 {
                continue;
            }

            match layer.combine {
                MaskCombine::Add => {
                    let (knee, boost) = layer.tone(base);
                    let mask_gain = 1.0 + (boost - 1.0) * curve(level, knee);
                    gain += weight * (mask_gain - gain);
                    coverage += weight * (1.0 - coverage);
                }
                MaskCombine::Intersect => {
                    // Only reaches pixels already covered underneath.
                    let weight = weight * coverage;
                    if weight <= 0.0 {
                        continue;
                    }
                    let (knee, boost) = layer.tone(base);
                    let mask_gain = 1.0 + (boost - 1.0) * curve(level, knee);
                    gain += weight * (mask_gain - gain);
                    coverage += weight * (1.0 - coverage);
                }
                MaskCombine::Subtract => {
                    // Pull back toward "untouched" instead of toward black.
                    gain += weight * (1.0 - gain);
                    coverage *= 1.0 - weight;
                }
            }
        }

        gain
    }

    /// Bake one layer's buffer, for showing that layer on its own.
    pub fn prepare_one(&self, index: usize, width: u32, height: u32) -> Option<Vec<f32>> {
        match &self.layers.get(index)?.shape {
            MaskShape::Brush(brush) if !brush.is_empty() => Some(brush.rasterize(width, height)),
            _ => None,
        }
    }

    /// One layer's weight at a pixel, without folding the rest of the stack.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn weight_of_layer(
        &self,
        index: usize,
        buffer: Option<&[f32]>,
        pixel: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        level: f32,
        rgb: [u8; 3],
    ) -> f32 {
        let Some(layer) = self.layers.get(index) else {
            return 0.0;
        };
        if !layer.enabled || layer.opacity <= 0.0 {
            return 0.0;
        }
        let raw = layer
            .shape
            .weight_at(x, y, width, height, level, rgb, buffer, pixel);
        let raw = if layer.invert { 1.0 - raw } else { raw };
        (raw * layer.opacity).clamp(0.0, 1.0)
    }

    /// Combined coverage of the stack at one pixel, ignoring the per-layer
    /// curve. This is what the editor draws as the mask overlay: it answers
    /// "how much does anything here reach", not "how bright will it get".
    #[inline]
    pub fn weight_at(
        &self,
        prepared: &[Option<Vec<f32>>],
        index: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        level: f32,
        rgb: [u8; 3],
    ) -> f32 {
        let mut coverage = 0.0f32;
        for (layer, buffer) in self.layers.iter().zip(prepared.iter()) {
            if !layer.enabled || layer.opacity <= 0.0 {
                continue;
            }
            let raw = layer.shape.weight_at(
                x,
                y,
                width,
                height,
                level,
                rgb,
                buffer.as_deref(),
                index,
            );
            let raw = if layer.invert { 1.0 - raw } else { raw };
            let weight = (raw * layer.opacity).clamp(0.0, 1.0);
            if weight <= 0.0 {
                continue;
            }
            match layer.combine {
                MaskCombine::Subtract => coverage *= 1.0 - weight,
                MaskCombine::Intersect => {
                    let weight = weight * coverage;
                    coverage += weight * (1.0 - coverage);
                }
                MaskCombine::Add => coverage += weight * (1.0 - coverage),
            }
        }
        coverage
    }

    // ---- persistence ------------------------------------------------------

    /// One-line encoding for the config file.
    ///
    /// `~` separates layers, `;` separates strokes. Numbers use fixed decimals
    /// so the text is stable across runs (the config writer compares strings).
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for (index, layer) in self.layers.iter().enumerate() {
            if index > 0 {
                out.push('~');
            }
            out.push(if layer.enabled { '1' } else { '0' });
            out.push('|');
            out.push(layer.combine.code());
            out.push('|');
            out.push(if layer.invert { '1' } else { '0' });
            out.push('|');
            out.push_str(&format!(
                "{:.3}|{:.3}|{:.3}|{}|{}|",
                layer.opacity, layer.knee, layer.max_boost,
                if layer.follow_base { '1' } else { '0' },
                sanitize(&layer.name),
            ));
            encode_shape(&layer.shape, &mut out);
        }
        out
    }

    /// Inverse of [`Self::encode`]. A malformed layer is skipped rather than
    /// failing the whole config.
    pub fn decode(text: &str) -> Self {
        let mut stack = MaskStack::default();
        for chunk in text.split('~') {
            if let Some(layer) = decode_layer(chunk) {
                stack.push(layer);
            }
        }
        stack
    }

    /// Import the pre-0.1.4 `paint=` line, which was one flat stroke list with
    /// no per-layer curve.
    pub fn from_legacy_paint(text: &str) -> Self {
        let mut strokes = BrushMask::default();
        let mut peak = 0.0f32;
        for chunk in text.split('~') {
            let Some((flag, rest)) = chunk.split_once('|') else {
                continue;
            };
            let mut parts = rest.split(',');
            let (Some(gain), Some(radius)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(gain), Ok(radius)) = (gain.parse::<f32>(), radius.parse::<f32>()) else {
                continue;
            };
            let mut stroke = BrushStroke {
                erase: flag.trim() == "1",
                radius: radius.clamp(0.0005, 0.5),
                hardness: 0.9,
                flow: 1.0,
                points: Vec::new(),
            };
            for pair in parts {
                let Some((u, v)) = pair.split_once(':') else {
                    continue;
                };
                let (Ok(u), Ok(v)) = (u.parse::<f32>(), v.parse::<f32>()) else {
                    continue;
                };
                if stroke.points.len() < MAX_POINTS_PER_STROKE {
                    stroke.points.push([u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)]);
                }
            }
            if stroke.points.is_empty() {
                continue;
            }
            if !stroke.erase {
                peak = peak.max(gain.clamp(0.0, 1.0));
            }
            strokes.push(stroke);
        }

        let mut stack = MaskStack::default();
        if !strokes.is_empty() {
            let mut layer = MaskLayer::new("画笔", MaskShape::Brush(strokes));
            // The old strokes carried a 0..1 fraction of the boost; turn the
            // strongest one into an absolute ceiling so the migration keeps
            // roughly the same look. 4.0 is the ceiling the old code could
            // reach at most.
            layer.follow_base = false;
            layer.knee = 0.55;
            layer.max_boost = 1.0 + peak * 3.0;
            stack.push(layer);
        }
        stack
    }
}

fn encode_shape(shape: &MaskShape, out: &mut String) {
    match shape {
        MaskShape::Linear {
            start,
            end,
            feather,
        } => {
            out.push_str(&format!(
                "L,{:.5},{:.5},{:.5},{:.5},{:.3}",
                start[0], start[1], end[0], end[1], feather
            ));
        }
        MaskShape::Radial {
            center,
            radius,
            rotation,
            feather,
        } => {
            out.push_str(&format!(
                "R,{:.5},{:.5},{:.5},{:.5},{:.4},{:.3}",
                center[0], center[1], radius[0], radius[1], rotation, feather
            ));
        }
        MaskShape::Brush(brush) => {
            out.push_str("B,");
            for (index, stroke) in brush.strokes.iter().enumerate() {
                if index > 0 {
                    out.push(';');
                }
                out.push(if stroke.erase { '1' } else { '0' });
                out.push(',');
                out.push_str(&format!(
                    "{:.4},{:.3},{:.3}",
                    stroke.radius, stroke.hardness, stroke.flow
                ));
                for point in &stroke.points {
                    out.push_str(&format!(",{:.5}:{:.5}", point[0], point[1]));
                }
            }
        }
        MaskShape::Luminance {
            min,
            max,
            softness,
        } => {
            out.push_str(&format!("Y,{:.3},{:.3},{:.3}", min, max, softness));
        }
        MaskShape::Color {
            hue,
            hue_tolerance,
            saturation_min,
            softness,
        } => {
            out.push_str(&format!(
                "C,{:.4},{:.4},{:.3},{:.3}",
                hue, hue_tolerance, saturation_min, softness
            ));
        }
    }
}

fn decode_layer(chunk: &str) -> Option<MaskLayer> {
    let mut fields = chunk.split('|');
    let enabled = fields.next()?.trim() == "1";
    let combine = MaskCombine::from_code(fields.next()?.chars().next()?);
    let invert = fields.next()?.trim() == "1";
    let opacity = fields.next()?.parse::<f32>().ok()?.clamp(0.0, 1.0);
    let knee = fields.next()?.parse::<f32>().ok()?.clamp(0.0, 0.999);
    let max_boost = fields.next()?.parse::<f32>().ok()?.clamp(1.0, 16.0);
    let follow_base = fields.next()?.trim() == "1";
    let name = fields.next()?.to_owned();
    let rest: Vec<&str> = fields.collect();
    let shape = decode_shape(&rest.join("|"))?;

    Some(MaskLayer {
        name: if name.trim().is_empty() {
            shape.label().to_owned()
        } else {
            name
        },
        enabled,
        invert,
        opacity,
        combine,
        knee,
        max_boost,
        follow_base,
        shape,
    })
}

fn decode_shape(text: &str) -> Option<MaskShape> {
    let (tag, rest) = text.split_once(',')?;
    let mut fields = rest.split(',');
    let mut next = || fields.next().and_then(|v| v.trim().parse::<f32>().ok());
    match tag.trim() {
        "L" => Some(MaskShape::Linear {
            start: [next()?.clamp(0.0, 1.0), next()?.clamp(0.0, 1.0)],
            end: [next()?.clamp(0.0, 1.0), next()?.clamp(0.0, 1.0)],
            feather: next()?.clamp(0.0, 4.0),
        }),
        "R" => Some(MaskShape::Radial {
            center: [next()?.clamp(0.0, 1.0), next()?.clamp(0.0, 1.0)],
            radius: [next()?.clamp(0.001, 2.0), next()?.clamp(0.001, 2.0)],
            rotation: next()?,
            feather: next()?.clamp(0.01, 0.95),
        }),
        "B" => {
            let mut brush = BrushMask::default();
            for chunk in rest.split(';') {
                let Some((flag, points)) = chunk.split_once(',') else {
                    continue;
                };
                let mut values = points.split(',');
                let (Some(radius), Some(hardness), Some(flow)) =
                    (values.next(), values.next(), values.next())
                else {
                    continue;
                };
                let (Ok(radius), Ok(hardness), Ok(flow)) = (
                    radius.parse::<f32>(),
                    hardness.parse::<f32>(),
                    flow.parse::<f32>(),
                ) else {
                    continue;
                };
                let mut stroke = BrushStroke {
                    erase: flag.trim() == "1",
                    radius: radius.clamp(0.0005, 0.5),
                    hardness: hardness.clamp(0.0, 1.0),
                    flow: flow.clamp(0.0, 1.0),
                    points: Vec::new(),
                };
                for pair in values {
                    let Some((u, v)) = pair.split_once(':') else {
                        continue;
                    };
                    let (Ok(u), Ok(v)) = (u.parse::<f32>(), v.parse::<f32>()) else {
                        continue;
                    };
                    if stroke.points.len() < MAX_POINTS_PER_STROKE {
                        stroke.points.push([u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)]);
                    }
                }
                if !stroke.points.is_empty() {
                    brush.push(stroke);
                }
            }
            Some(MaskShape::Brush(brush))
        }
        "Y" => Some(MaskShape::Luminance {
            min: next()?.clamp(0.0, 1.0),
            max: next()?.clamp(0.0, 1.0),
            softness: next()?.clamp(0.0, 1.0),
        }),
        "C" => Some(MaskShape::Color {
            hue: next()?.rem_euclid(1.0),
            hue_tolerance: next()?.clamp(0.001, 0.5),
            saturation_min: next()?.clamp(0.0, 0.99),
            softness: next()?.clamp(0.0, 1.0),
        }),
        _ => None,
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '|' | '~' | ';' | '\n' | '\r' => ' ',
            other => other,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

// ---- shared maths ---------------------------------------------------------

#[inline]
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Fraction of the available boost the curve asks for at `level`.
#[inline]
pub fn curve(level: f32, knee: f32) -> f32 {
    let knee = knee.clamp(0.0, 0.999);
    let span = (1.0 - knee).max(1e-6);
    smoothstep(((level - knee) / span).clamp(0.0, 1.0))
}

/// `(hue, saturation, lightness)` with hue on `0..1`.
pub fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = 0.5 * (max + min);
    let delta = max - min;
    if delta < 1e-6 {
        return (0.0, 0.0, lightness);
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs()).max(1e-6);
    let hue = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    (hue / 6.0, saturation.clamp(0.0, 1.0), lightness)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ToneParams {
        ToneParams {
            knee: 0.55,
            max_boost: 3.0,
            top_level: 1.0,
            top_stops: 1.5,
        }
    }

    #[test]
    fn linear_gradient_is_monotone_along_its_axis() {
        let shape = MaskShape::Linear {
            start: [0.2, 0.5],
            end: [0.8, 0.5],
            feather: 0.0,
        };
        let mut previous = -1.0;
        for step in 0..=20 {
            let x = step as f32 / 20.0 * 100.0;
            let w = shape.weight_at(x, 50.0, 100.0, 100.0, 0.5, [0, 0, 0], None, 0);
            assert!(w >= previous - 1e-6, "weight went backwards at x={x}");
            previous = w;
        }
        assert!((previous - 1.0).abs() < 1e-6);
        let leading = shape.weight_at(0.0, 50.0, 100.0, 100.0, 0.5, [0, 0, 0], None, 0);
        assert!(leading < 1e-6);
    }

    #[test]
    fn feather_pushes_the_ramp_back_past_the_start() {
        let sharp = MaskShape::Linear {
            start: [0.5, 0.5],
            end: [0.9, 0.5],
            feather: 0.0,
        };
        let soft = MaskShape::Linear {
            start: [0.5, 0.5],
            end: [0.9, 0.5],
            feather: 2.0,
        };
        let at_start = |s: &MaskShape| s.weight_at(50.0, 50.0, 100.0, 100.0, 0.5, [0; 3], None, 0);
        assert!(at_start(&sharp) < 1e-6);
        assert!(at_start(&soft) > 0.3);
    }

    #[test]
    fn radial_is_full_inside_and_empty_outside() {
        let shape = MaskShape::Radial {
            center: [0.5, 0.5],
            radius: [0.25, 0.25],
            rotation: 0.0,
            feather: 0.2,
        };
        let sample = |x: f32, y: f32| {
            shape.weight_at(x, y, 100.0, 100.0, 0.5, [0; 3], None, 0)
        };
        assert!((sample(50.0, 50.0) - 1.0).abs() < 1e-6);
        assert!(sample(90.0, 90.0) < 1e-6);
        // Rotation must not change a circle.
        let rotated = MaskShape::Radial {
            center: [0.5, 0.5],
            radius: [0.25, 0.25],
            rotation: 1.1,
            feather: 0.2,
        };
        let a = shape.weight_at(70.0, 50.0, 100.0, 100.0, 0.5, [0; 3], None, 0);
        let b = rotated.weight_at(70.0, 50.0, 100.0, 100.0, 0.5, [0; 3], None, 0);
        assert!((a - b).abs() < 1e-5);
    }

    #[test]
    fn luminance_band_selects_only_its_range() {
        let shape = MaskShape::Luminance {
            min: 0.4,
            max: 0.7,
            softness: 0.0,
        };
        let at = |level: f32| shape.weight_at(0.0, 0.0, 10.0, 10.0, level, [0; 3], None, 0);
        assert!(at(0.2) < 1e-6);
        assert!((at(0.55) - 1.0).abs() < 1e-6);
        assert!(at(0.9) < 1e-6);
    }

    #[test]
    fn colour_mask_ignores_grey_pixels() {
        let shape = MaskShape::Color {
            hue: 0.0,
            hue_tolerance: 0.1,
            saturation_min: 0.3,
            softness: 0.0,
        };
        let red = shape.weight_at(0.0, 0.0, 10.0, 10.0, 0.5, [220, 20, 20], None, 0);
        let grey = shape.weight_at(0.0, 0.0, 10.0, 10.0, 0.5, [128, 128, 128], None, 0);
        let blue = shape.weight_at(0.0, 0.0, 10.0, 10.0, 0.5, [20, 20, 220], None, 0);
        // A saturated red reaches most of the way; the saturation ramp is what
        // costs the remaining few percent.
        assert!(red > 0.8, "red {red}");
        assert!(grey < 1e-6);
        assert!(blue < 1e-6);
    }

    #[test]
    fn empty_stack_reproduces_the_base_gain_exactly() {
        let stack = MaskStack::default();
        let prepared = stack.prepare(4, 4);
        let base = base();
        let gain = stack.gain_at(&prepared, 0, 0.0, 0.0, 4.0, 4.0, 0.8, [10, 20, 30], 2.5, base);
        assert_eq!(gain, 2.5);
    }

    #[test]
    fn add_layer_lerps_toward_its_own_gain() {
        let mut layer = MaskLayer::new(
            "全局",
            MaskShape::Luminance {
                min: 0.0,
                max: 1.0,
                softness: 1.0,
            },
        );
        layer.follow_base = false;
        layer.max_boost = 4.0;
        layer.knee = 0.0;
        let mut stack = MaskStack::default();
        stack.push(layer);

        let prepared = stack.prepare(1, 1);
        let base = base();
        // At full boost the layer wants 4.0; half-opacity lands halfway.
        let full = stack.gain_at(&prepared, 0, 0.0, 0.0, 1.0, 1.0, 1.0, [0; 3], 2.0, base);
        assert!((full - 4.0).abs() < 0.05, "got {full}");

        stack.layers_mut()[0].opacity = 0.5;
        let half = stack.gain_at(&prepared, 0, 0.0, 0.0, 1.0, 1.0, 1.0, [0; 3], 2.0, base);
        assert!((half - 3.0).abs() < 0.05, "got {half}");
    }

    #[test]
    fn subtract_layer_never_goes_below_one() {
        let mut layer = MaskLayer::new(
            "保护",
            MaskShape::Luminance {
                min: 0.0,
                max: 1.0,
                softness: 1.0,
            },
        );
        layer.combine = MaskCombine::Subtract;
        layer.opacity = 1.0;
        let mut stack = MaskStack::default();
        stack.push(layer);
        let prepared = stack.prepare(1, 1);
        let gain = stack.gain_at(&prepared, 0, 0.0, 0.0, 1.0, 1.0, 1.0, [0; 3], 3.5, base());
        assert!((gain - 1.0).abs() < 1e-5, "got {gain}");
    }

    #[test]
    fn intersect_only_reaches_pixels_covered_below() {
        let bright = MaskShape::Luminance {
            min: 0.9,
            max: 1.0,
            softness: 0.0,
        };
        let dim = MaskShape::Luminance {
            min: 0.0,
            max: 0.2,
            softness: 0.0,
        };
        let mut first = MaskLayer::new("亮部", bright);
        first.follow_base = false;
        first.max_boost = 4.0;
        first.knee = 0.0;
        let mut second = MaskLayer::new("交集", dim);
        second.combine = MaskCombine::Intersect;
        second.follow_base = false;
        second.max_boost = 1.0;
        second.knee = 0.0;

        let mut stack = MaskStack::default();
        stack.push(first);
        stack.push(second);
        let prepared = stack.prepare(1, 1);
        // A dim pixel is not covered by the first layer, so the intersect layer
        // must do nothing at all.
        let gain = stack.gain_at(&prepared, 0, 0.0, 0.0, 1.0, 1.0, 0.1, [0; 3], 2.0, base());
        assert!((gain - 2.0).abs() < 1e-5, "got {gain}");
    }

    #[test]
    fn encode_decode_round_trips_every_shape() {
        let mut stack = MaskStack::default();
        stack.push(MaskLayer::new(
            "线",
            MaskShape::Linear {
                start: [0.1, 0.2],
                end: [0.9, 0.8],
                feather: 1.5,
            },
        ));
        let mut radial = MaskLayer::new(
            "圆",
            MaskShape::Radial {
                center: [0.5, 0.4],
                radius: [0.3, 0.2],
                rotation: 0.7,
                feather: 0.4,
            },
        );
        radial.combine = MaskCombine::Subtract;
        radial.invert = true;
        radial.opacity = 0.6;
        radial.follow_base = false;
        radial.knee = 0.3;
        radial.max_boost = 2.2;
        stack.push(radial);

        let mut brush = BrushMask::default();
        let mut stroke = BrushStroke::new(0.05, 0.8, 0.9, false, [0.2, 0.3]);
        stroke.push([0.4, 0.5]);
        brush.push(stroke);
        stack.push(MaskLayer::new("笔", MaskShape::Brush(brush)));
        stack.push(MaskLayer::new(
            "亮",
            MaskShape::Luminance {
                min: 0.2,
                max: 0.8,
                softness: 0.5,
            },
        ));
        stack.push(MaskLayer::new(
            "红",
            MaskShape::Color {
                hue: 0.05,
                hue_tolerance: 0.12,
                saturation_min: 0.4,
                softness: 0.3,
            },
        ));

        let text = stack.encode();
        let back = MaskStack::decode(&text);
        assert_eq!(back.len(), stack.len());
        assert_eq!(back.encode(), text);
        for (a, b) in stack.layers().iter().zip(back.layers()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.combine, b.combine);
            assert_eq!(a.invert, b.invert);
            assert!((a.opacity - b.opacity).abs() < 1e-3);
            assert_eq!(a.follow_base, b.follow_base);
        }
    }

    #[test]
    fn malformed_config_does_not_lose_good_layers() {
        let good = MaskStack::decode("1|a|0|1.000|0.550|3.000|1|好|Y,0.100,0.900,0.200");
        assert_eq!(good.len(), 1);
        let mixed = MaskStack::decode("garbage~1|a|0|1.000|0.550|3.000|1|好|Y,0.100,0.900,0.200~1|a|0");
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn legacy_paint_text_becomes_a_brush_layer() {
        let stack = MaskStack::from_legacy_paint(
            "0|1.000,0.0600,0.20000:0.30000,0.35000:0.32000~1|0.000,0.0400,0.90000:0.90000",
        );
        assert_eq!(stack.len(), 1);
        match &stack.layers()[0].shape {
            MaskShape::Brush(brush) => {
                assert_eq!(brush.strokes.len(), 2);
                assert!(brush.strokes[1].erase);
                assert_eq!(brush.strokes[0].points.len(), 2);
            }
            other => panic!("unexpected shape {other:?}"),
        }
        assert!(stack.has_effect());
    }

    #[test]
    fn brush_rasterize_lands_on_the_drawn_rows() {
        let mut brush = BrushMask::default();
        let mut stroke = BrushStroke::new(0.05, 1.0, 1.0, false, [0.1, 0.5]);
        stroke.push([0.9, 0.5]);
        brush.push(stroke);
        let (w, h) = (200u32, 100u32);
        let buffer = brush.rasterize(w, h);
        let touched: Vec<usize> = (0..h as usize)
            .filter(|row| {
                let start = row * w as usize;
                buffer[start..start + w as usize].iter().any(|&v| v > 0.0)
            })
            .collect();
        assert!(!touched.is_empty());
        // Radius 0.05 * 200 = 10 px around row 50.
        assert!(*touched.first().unwrap() >= 38, "top {touched:?}");
        assert!(*touched.last().unwrap() <= 62, "bottom {touched:?}");
        let middle = 50 * w as usize + 100;
        assert!((buffer[middle] - 1.0).abs() < 1e-5);
        let corner = 2 * w as usize + 2;
        assert!(buffer[corner] < 1e-6);
    }
}
