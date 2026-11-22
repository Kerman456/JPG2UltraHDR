//! Mask editing: the panel that owns the layer list, and the preview surface
//! that turns mouse gestures into shapes and brush strokes.
//!
//! # Why the preview is a separate resolution
//!
//! A 20 MP picture takes about 1.5 s to convert, which is far too slow to run
//! while the pointer is moving. So the editor keeps a 1000-pixel-wide copy of
//! the frame ([`jpg2ultrahdr::PreviewFrame`]) and evaluates masks against *that*
//! buffer during a drag. The full-resolution gain map is rebuilt only when the
//! gesture ends. Both use the same normalized geometry, so what is drawn is what
//! gets exported.
//!
//! # Interaction
//!
//! | gesture | effect |
//! |---|---|
//! | drag with the Linear tool | places start and end of a gradient |
//! | drag with the Radial tool | centre, then radius |
//! | drag with the Brush tool | paints; consecutive samples become one stroke |
//! | wheel over the preview | feather (Linear/Radial), brush size (Brush) |
//! | `[` / `]` | same as the wheel, keyboard |
//! | right-drag | same as the wheel |
//! | middle-drag / space-drag | pan the view when zoomed in |

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use jpg2ultrahdr::{
    BrushMask, BrushStroke, MaskCombine, MaskLayer, MaskShape, MaskStack, PreviewFrame, ToneParams,
};
use modern_egui::theme::ui_ext::PanelVariant;
use modern_egui::theme::{UiButtons, UiInputs, UiMetrics, UiPanels, UiText};

/// Which shape the next drag creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Linear,
    Radial,
    Brush,
    Luminance,
    Color,
}

impl Tool {
    pub const ALL: [Self; 5] = [
        Self::Linear,
        Self::Radial,
        Self::Brush,
        Self::Luminance,
        Self::Color,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "线性",
            Self::Radial => "径向",
            Self::Brush => "画笔",
            Self::Luminance => "亮度",
            Self::Color => "颜色",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Linear => "在预览图上按住拖一条线：起点不动，终点提亮最多。滚轮调羽化。",
            Self::Radial => "按住从中心向外拖出椭圆范围。滚轮调边缘柔和度。",
            Self::Brush => "在预览图上涂抹。滚轮（或双指捏合）调笔刷大小。",
            Self::Luminance => "按亮度取样：把指针移到画面上，点一下即取该处的明暗范围。",
            Self::Color => "按颜色取样：点画面上的颜色即可选中同色区域。",
        }
    }

    fn shape_at(self, point: [f32; 2], level: f32, rgb: [u8; 3]) -> MaskShape {
        match self {
            Self::Linear => MaskShape::Linear {
                start: point,
                end: point,
                feather: 1.0,
            },
            Self::Radial => MaskShape::Radial {
                center: point,
                radius: [0.0001, 0.0001],
                rotation: 0.0,
                feather: 0.5,
            },
            Self::Brush => MaskShape::Brush(BrushMask::default()),
            Self::Luminance => {
                let band = 0.12;
                MaskShape::Luminance {
                    min: (level - band).clamp(0.0, 1.0),
                    max: (level + band).clamp(0.0, 1.0),
                    softness: 0.35,
                }
            }
            Self::Color => {
                let (hue, saturation, _) = jpg2ultrahdr::rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
                MaskShape::Color {
                    hue,
                    hue_tolerance: 0.08,
                    saturation_min: (saturation * 0.4).clamp(0.0, 0.5),
                    softness: 0.3,
                }
            }
        }
    }
}

/// A gesture in progress.
pub enum Draft {
    /// Linear/Radial: the anchor is kept, the pointer supplies the other end.
    Shape {
        layer: usize,
        anchor: [f32; 2],
    },
    /// Brush: samples accumulate until the button comes up.
    Stroke(BrushStroke),
}

/// What the preview should draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The mask stack, composited.
    All,
    /// Just the selected layer.
    Selected,
    /// No overlay: show the HDR simulation instead.
    None,
}

/// All editor state the panel needs.
pub struct MaskEditor {
    pub tool: Tool,
    pub overlay: Overlay,
    pub selected: usize,
    /// Show the simulated HDR rendering instead of the plain SDR preview.
    pub simulate: bool,
    pub exposure: f32,
    /// Brush radius in units of the image width.
    pub brush_radius: f32,
    pub brush_hardness: f32,
    pub brush_flow: f32,
    pub brush_erase: bool,
    pub draft: Option<Draft>,
    /// Layer being renamed, plus its edit buffer.
    pub renaming: Option<(usize, String)>,
    /// Preview textures, rebuilt only when their inputs change.
    textures: Textures,
}

#[derive(Default)]
struct Textures {
    base: Option<egui::TextureHandle>,
    overlay: Option<egui::TextureHandle>,
    /// The mask stack and the settings that produced `overlay`.
    overlay_key: String,
    overlay_size: [usize; 2],
}

impl Default for MaskEditor {
    fn default() -> Self {
        Self {
            tool: Tool::Linear,
            overlay: Overlay::All,
            selected: 0,
            simulate: false,
            exposure: 1.0,
            brush_radius: 0.06,
            brush_hardness: 0.7,
            brush_flow: 1.0,
            brush_erase: false,
            draft: None,
            renaming: None,
            textures: Textures::default(),
        }
    }
}

impl MaskEditor {
    /// Forget cached textures: the picture changed.
    ///
    /// The selected layer is deliberately kept: masks are a global stack that
    /// spans the whole batch, so switching pictures should not yank the panel
    /// back to layer 0. [`Self::clamp_selection`] handles the case where the
    /// stack shrank underneath us.
    pub fn reset(&mut self) {
        self.textures = Textures::default();
        self.draft = None;
        self.renaming = None;
    }

    /// Keep [`Self::selected`] inside the stack after layers are removed or a
    /// settings file asks for a layer that no longer exists.
    pub fn clamp_selection(&mut self, masks: &MaskStack) {
        self.selected = self.selected.min(masks.len().saturating_sub(1));
    }

    fn overlay_key(&self, masks: &MaskStack, tone: ToneParams) -> String {
        let mut key = masks.encode();
        key.push('#');
        key.push_str(match self.overlay {
            Overlay::All => "all",
            Overlay::Selected => "one",
            Overlay::None => "hdr",
        });
        key.push('#');
        key.push_str(&format!("{}", self.selected));
        if self.overlay == Overlay::None {
            // The simulation also depends on the curve, which the masks do not
            // record when every layer follows the base.
            key.push_str(&format!(
                "#{:.4}:{:.4}:{:.3}",
                tone.max_boost, tone.knee, self.exposure
            ));
        }
        key
    }

    /// Rebuild the overlay texture if anything it depends on changed.
    fn ensure_textures(
        &mut self,
        ctx: &egui::Context,
        frame: Option<&PreviewFrame>,
        masks: &MaskStack,
        tone: ToneParams,
    ) {
        let Some(frame) = frame else {
            self.textures = Textures::default();
            return;
        };
        let size = [frame.width as usize, frame.height as usize];

        if self.textures.base.is_none() || self.textures.overlay_size != size {
            let image = egui::ColorImage::from_rgb(size, &frame.rgb);
            self.textures.base =
                Some(ctx.load_texture("mask_base", image, egui::TextureOptions::LINEAR));
        }

        let key = self.overlay_key(masks, tone);
        if self.textures.overlay.is_some()
            && self.textures.overlay_key == key
            && self.textures.overlay_size == size
        {
            return;
        }

        let rgba = match self.overlay {
            Overlay::All => frame.overlay_rgba(masks),
            Overlay::Selected => frame.overlay_one_rgba(masks, self.selected),
            Overlay::None => frame.simulated_rgba(masks, tone, self.exposure),
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
        self.textures.overlay =
            Some(ctx.load_texture("mask_overlay", image, egui::TextureOptions::LINEAR));
        self.textures.overlay_key = key;
        self.textures.overlay_size = size;
    }

    /// Texture to show: the overlay when there is one, else the plain picture.
    fn texture(&self) -> Option<egui::TextureId> {
        self.textures
            .overlay
            .as_ref()
            .or(self.textures.base.as_ref())
            .map(|t| t.id())
    }
}

/// Everything the editor needs from the app while drawing.
pub struct PreviewContext<'a> {
    pub frame: Option<&'a PreviewFrame>,
    pub masks: &'a mut Arc<MaskStack>,
    pub tone: ToneParams,
}

impl MaskEditor {
    /// The preview surface: image, gestures, overlay, and the "commit" signal.
    ///
    /// Returns `true` when the gesture ended and the caller should rebuild the
    /// full-resolution gain map.
    pub fn show_preview(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        state: &mut PreviewContext<'_>,
    ) -> bool {
        self.ensure_textures(ctx, state.frame, state.masks, state.tone);

        let Some(frame) = state.frame else {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| ui.muted_label("选择左侧文件以编辑蒙版"));
            return false;
        };

        let available = ui.available_size();
        let aspect = frame.width as f32 / frame.height as f32;
        let mut shown = egui::vec2(available.x, available.x / aspect);
        if shown.y > available.y {
            shown = egui::vec2(available.y * aspect, available.y);
        }
        shown.x = shown.x.max(32.0);
        shown.y = shown.y.max(32.0);

        ui.add_space(((available.y - shown.y) * 0.5).max(0.0));
        let mut commit = false;
        ui.horizontal(|ui| {
            ui.add_space(((available.x - shown.x) * 0.5).max(0.0));
            let (rect, response) =
                ui.allocate_exact_size(shown, egui::Sense::click_and_drag());
            if let Some(id) = self.texture() {
                ui.painter().image(
                    id,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                ui.painter()
                    .rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
            }

            self.handle_input(ui, &response, rect, state);
            self.paint_handles(ui, rect, state);

            if self.draft.is_some() && response.drag_stopped() {
                self.commit(ui, state);
                commit = true;
            }
            // A click ends a dab drawn by the brush tool.
            if matches!(self.draft, Some(Draft::Stroke(_))) && response.clicked() {
                self.commit(ui, state);
                commit = true;
            }
        });
        commit
    }

    fn handle_input(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        state: &mut PreviewContext<'_>,
    ) {
        let to_norm = |pos: egui::Pos2| {
            [
                ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
                ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
            ]
        };
        // Sample the picture under the cursor, for the luminance/colour tools.
        // `frame` is copied out first so this closure does not hold a borrow of
        // `state` (which the size/feather handling below needs mutably).
        let frame = state.frame;
        let sample = |pos: egui::Pos2| -> (f32, [u8; 3]) {
            let Some(frame) = frame else {
                return (0.5, [128, 128, 128]);
            };
            let u = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let v = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
            let x = ((u * frame.width as f32) as u32).min(frame.width.saturating_sub(1)) as usize;
            let y = ((v * frame.height as f32) as u32).min(frame.height.saturating_sub(1)) as usize;
            let index = (y * frame.width as usize + x) * 3;
            let rgb = [
                frame.rgb[index],
                frame.rgb[index + 1],
                frame.rgb[index + 2],
            ];
            (frame.level_at(x, y), rgb)
        };

        // ---- size / feather ------------------------------------------------
        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                self.adjust_size(state, scroll.signum());
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::OpenBracket)) {
            self.adjust_size(state, -1.0);
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::CloseBracket)) {
            self.adjust_size(state, 1.0);
        }

        // ---- gesture start -------------------------------------------------
        if response.drag_started() || response.clicked() {
            let Some(pos) = response.interact_pointer_pos() else {
                return;
            };
            let point = to_norm(pos);

            match self.tool {
                Tool::Brush => {
                    if let MaskShape::Brush(brush) = &state.masks.layers()[self.selected].shape {
                        if !brush.is_empty() {
                            // The selected layer already has strokes: keep
                            // painting into it instead of stacking a new layer.
                            let stroke = BrushStroke::new(
                                self.brush_radius,
                                self.brush_hardness,
                                self.brush_flow,
                                self.brush_erase,
                                point,
                            );
                            self.draft = Some(Draft::Stroke(stroke));
                            return;
                        }
                    }
                    let mut layer = MaskLayer::new(
                        format!("画笔 {}", state.masks.len() + 1),
                        MaskShape::Brush(BrushMask::default()),
                    );
                    // A brush is a "point at it" tool: start with a boost the
                    // user can see, then let them tune it.
                    layer.follow_base = false;
                    layer.max_boost = state.tone.max_boost;
                    layer.knee = (state.tone.knee - 0.15).clamp(0.0, 0.9);
                    Arc::make_mut(state.masks).push(layer);
                    self.select_last(state);
                    self.draft = Some(Draft::Stroke(BrushStroke::new(
                        self.brush_radius,
                        self.brush_hardness,
                        self.brush_flow,
                        self.brush_erase,
                        point,
                    )));
                }
                Tool::Linear | Tool::Radial => {
                    let (level, rgb) = sample(pos);
                    let shape = self.tool.shape_at(point, level, rgb);
                    let mut layer =
                        MaskLayer::new(format!("{} {}", self.tool.label(), state.masks.len() + 1), shape);
                    layer.follow_base = false;
                    layer.max_boost = state.tone.max_boost;
                    layer.knee = (state.tone.knee - 0.15).clamp(0.0, 0.9);
                    Arc::make_mut(state.masks).push(layer);
                    self.select_last(state);
                    self.draft = Some(Draft::Shape {
                        layer: self.selected,
                        anchor: point,
                    });
                }
                Tool::Luminance | Tool::Color => {
                    let (level, rgb) = sample(pos);
                    let shape = self.tool.shape_at(point, level, rgb);
                    let mut layer =
                        MaskLayer::new(format!("{} {}", self.tool.label(), state.masks.len() + 1), shape);
                    layer.follow_base = false;
                    layer.max_boost = state.tone.max_boost;
                    layer.knee = (state.tone.knee - 0.15).clamp(0.0, 0.9);
                    Arc::make_mut(state.masks).push(layer);
                    self.select_last(state);
                }
            }
        }

        // ---- gesture update ------------------------------------------------
        if response.dragged() {
            let Some(pos) = response.interact_pointer_pos() else {
                return;
            };
            let point = to_norm(pos);
            match &mut self.draft {
                Some(Draft::Shape { layer, anchor }) => {
                    let anchor = *anchor;
                    if let Some(target) = Arc::make_mut(state.masks).layers_mut().get_mut(*layer) {
                        match &mut target.shape {
                            MaskShape::Linear { start, end, .. } => {
                                *start = anchor;
                                *end = point;
                            }
                            MaskShape::Radial {
                                center, radius, ..
                            } => {
                                let dx = (point[0] - anchor[0]).abs();
                                let dy = (point[1] - anchor[1]).abs();
                                *center = anchor;
                                radius[0] = dx.max(0.005);
                                radius[1] = dy.max(0.005);
                            }
                            _ => {}
                        }
                    }
                }
                Some(Draft::Stroke(stroke)) => {
                    // `dragged()` fires once per frame, so a quick flick leaves
                    // gaps wider than the brush. Fill them in.
                    if let Some(last) = stroke.points.last().copied() {
                        let mut extra = Vec::new();
                        interpolate(last, point, self.brush_radius, &mut extra);
                        for sample in extra {
                            stroke.push(sample);
                        }
                    }
                    stroke.push(point);
                }
                None => {}
            }
        }
    }

    /// Wheel / bracket keys: the parameter that makes sense for the current
    /// tool — brush size, gradient feather, or preview exposure.
    fn adjust_size(&mut self, state: &mut PreviewContext<'_>, direction: f32) {
        match self.tool {
            Tool::Brush => {
                let factor = if direction > 0.0 { 1.12 } else { 1.0 / 1.12 };
                self.brush_radius = (self.brush_radius * factor).clamp(0.005, 0.5);
            }
            Tool::Linear | Tool::Radial => {
                let selected = self.selected;
                if let Some(layer) = Arc::make_mut(state.masks).layers_mut().get_mut(selected) {
                    let current = layer.shape.feather();
                    layer
                        .shape
                        .set_feather((current + direction * 0.05).clamp(0.0, 1.0));
                }
            }
            Tool::Luminance | Tool::Color => {
                self.exposure = (self.exposure + direction * 0.05).clamp(0.2, 4.0);
            }
        }
    }

    fn select_last(&mut self, state: &PreviewContext<'_>) {
        self.selected = state.masks.len().saturating_sub(1);
    }

    /// Turn the draft into a committed layer edit.
    fn commit(&mut self, _ui: &egui::Ui, state: &mut PreviewContext<'_>) {
        if let Some(draft) = self.draft.take() {
            match draft {
                Draft::Stroke(stroke) => {
                    if let Some(layer) =
                        Arc::make_mut(state.masks).layers_mut().get_mut(self.selected)
                    {
                        if let MaskShape::Brush(brush) = &mut layer.shape {
                            brush.push(stroke);
                        }
                    }
                }
                Draft::Shape { .. } => {}
            }
        }
    }

    /// Draw control points for the selected shape, plus the brush cursor.
    fn paint_handles(&self, ui: &egui::Ui, rect: egui::Rect, state: &PreviewContext<'_>) {
        let painter = ui.painter_at(rect);
        let to_screen = |p: [f32; 2]| {
            egui::pos2(
                rect.min.x + p[0] * rect.width(),
                rect.min.y + p[1] * rect.height(),
            )
        };
        let accent = egui::Color32::from_rgb(255, 176, 64);

        if let Some(layer) = state.masks.layers().get(self.selected) {
            match &layer.shape {
                MaskShape::Linear { start, end, .. } => {
                    let (a, b) = (to_screen(*start), to_screen(*end));
                    painter.line_segment([a, b], egui::Stroke::new(1.5, accent));
                    for point in [a, b] {
                        painter.circle_filled(point, 5.0, accent);
                        painter.circle_stroke(
                            point,
                            5.0,
                            egui::Stroke::new(1.0, egui::Color32::from_black_alpha(160)),
                        );
                    }
                }
                MaskShape::Radial {
                    center,
                    radius,
                    rotation,
                    ..
                } => {
                    let c = to_screen(*center);
                    let rx = radius[0] * rect.width();
                    let ry = radius[1] * rect.height();
                    // egui has no rotated ellipse primitive; a polygon through
                    // 48 points is exact enough at screen resolution.
                    let points: Vec<egui::Pos2> = (0..48)
                        .map(|step| {
                            let angle = std::f32::consts::TAU * step as f32 / 48.0;
                            let (sin, cos) = angle.sin_cos();
                            let (rs, rc) = rotation.sin_cos();
                            let x = cos * rx;
                            let y = sin * ry;
                            egui::pos2(c.x + x * rc - y * rs, c.y + x * rs + y * rc)
                        })
                        .collect();
                    painter.add(egui::Shape::closed_line(
                        points,
                        egui::Stroke::new(1.5, accent),
                    ));
                    painter.circle_filled(c, 4.0, accent);
                }
                _ => {}
            }
        }

        // Brush cursor.
        if self.tool == Tool::Brush {
            if let Some(pos) = ui.ctx().pointer_hover_pos() {
                if rect.contains(pos) {
                    let radius = (self.brush_radius * rect.width()).max(3.0);
                    painter.circle_stroke(pos, radius, egui::Stroke::new(1.5, accent));
                    painter.circle_stroke(
                        pos,
                        radius + 1.0,
                        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(140)),
                    );
                }
            }
        }
    }
}

/// Wrap the preview in a card with the toolbar above it.
pub fn preview_toolbar(editor: &mut MaskEditor, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        for tool in Tool::ALL {
            let selected = editor.tool == tool;
            if ui
                .selectable_label(selected, tool.label())
                .on_hover_text(tool.hint())
                .clicked()
            {
                editor.tool = tool;
                editor.draft = None;
            }
        }
        ui.separator();
        ui.checkbox(&mut editor.simulate, "HDR 预览")
            .on_hover_text("按当前曲线模拟 HDR 查看器的显示效果");
        if editor.simulate {
            ui.add(
                egui::Slider::new(&mut editor.exposure, 0.2..=4.0)
                    .text("曝光")
                    .fixed_decimals(2),
            );
        }
    });
    ui.horizontal(|ui| {
        ui.metadata_label("显示：");
        for (overlay, label) in [
            (Overlay::All, "全部蒙版"),
            (Overlay::Selected, "当前蒙版"),
            (Overlay::None, "无"),
        ] {
            if ui
                .selectable_label(editor.overlay == overlay, label)
                .clicked()
            {
                editor.overlay = overlay;
                editor.simulate = overlay == Overlay::None;
            }
        }
    });
}

/// The layer list and the selected layer's controls.
///
/// Returns `true` when the settings changed enough to invalidate the preview.
pub fn layer_panel(
    editor: &mut MaskEditor,
    ui: &mut egui::Ui,
    masks: &mut Arc<MaskStack>,
    tone: ToneParams,
) -> bool {
    // Comparing the encoded stacks is cheap next to a rebuild and catches every
    // control at once, including the sliders that write through `Arc::make_mut`.
    let before = masks.encode();

    ui.horizontal(|ui| {
        ui.section_title("蒙版");
        ui.metadata_label(&format!("{} 个", masks.len()));
    });
    ui.space_inline();

    if masks.is_empty() {
        ui.metadata_label("还没有蒙版。选一个工具，在预览图上拖一下就建好一个。");
        return false;
    }

    // ---- layer rows ------------------------------------------------------
    let count = masks.len();
    let mut remove: Option<usize> = None;
    let mut move_up: Option<usize> = None;
    let mut select: Option<usize> = None;
    for index in 0..count {
        let (enabled, name, combine, opacity) = {
            let layer = &masks.layers()[index];
            (layer.enabled, layer.name.clone(), layer.combine, layer.opacity)
        };
        let selected = editor.selected == index;
        let response = ui.horizontal(|ui| {
            let mut on = enabled;
            if ui.checkbox(&mut on, "").changed() {
                if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(index) {
                    layer.enabled = on;
                }
            }
            if editor.renaming.as_ref().map(|(i, _)| *i) == Some(index) {
                let mut buffer = editor
                    .renaming
                    .as_ref()
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default();
                let edit = ui.text_input(&mut buffer);
                if let Some((_, text)) = editor.renaming.as_mut() {
                    *text = buffer.clone();
                }
                if edit.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(index) {
                        if !buffer.trim().is_empty() {
                            layer.name = buffer.trim().to_owned();
                        }
                    }
                    editor.renaming = None;
                }
            } else {
                let text = format!("{}  {}", combine.label(), name);
                if ui
                    .selectable_label(selected, text)
                    .on_hover_text("双击重命名")
                    .clicked()
                {
                    select = Some(index);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.ghost_button("×").on_hover_text("删除").clicked() {
                    remove = Some(index);
                }
                if index + 1 < count
                    && ui.ghost_button("↓").on_hover_text("下移").clicked()
                {
                    move_up = Some(index);
                }
                if ui
                    .ghost_button("…")
                    .on_hover_text("重命名")
                    .clicked()
                {
                    editor.renaming = Some((index, name.clone()));
                }
                ui.metadata_label(&format!("{:.0}%", opacity * 100.0));
            });
        });
        // Double click on the row starts a rename.
        if response.response.double_clicked() {
            editor.renaming = Some((index, name.clone()));
        }
    }

    if let Some(index) = select {
        editor.selected = index;
    }
    if let Some(index) = remove {
        Arc::make_mut(masks).remove(index);
        editor.clamp_selection(masks);
        editor.draft = None;
    }
    if let Some(index) = move_up {
        let layers = Arc::make_mut(masks).layers_mut();
        if index + 1 < layers.len() {
            layers.swap(index, index + 1);
            editor.selected = index + 1;
        }
    }

    if masks.is_empty() {
        return masks.encode() != before;
    }
    editor.clamp_selection(masks);

    ui.space_group();

    // ---- selected layer --------------------------------------------------
    let base = tone;
    let selected = editor.selected;
    let (mut combine, mut invert, mut opacity, mut follow, mut knee, mut boost, shape_label) = {
        let layer = &masks.layers()[selected];
        (
            layer.combine,
            layer.invert,
            layer.opacity,
            layer.follow_base,
            layer.knee,
            layer.max_boost,
            layer.shape.label(),
        )
    };

    let full = ui.available_width();
    ui.card_with_variant(PanelVariant::SurfaceAlt, |ui| {
        ui.set_min_width(full - 30.0);
        ui.card_title(&format!("「{}」· {}", masks.layers()[selected].name, shape_label));
        ui.space_group();

        ui.horizontal(|ui| {
            ui.metadata_label("组合");
            for option in [
                MaskCombine::Add,
                MaskCombine::Subtract,
                MaskCombine::Intersect,
            ] {
                if ui
                    .selectable_label(combine == option, option.label())
                    .clicked()
                {
                    combine = option;
                }
            }
        });
        ui.checkbox(&mut invert, "反转");

        let mut percent = opacity * 100.0;
        if ui
            .add(egui::Slider::new(&mut percent, 0.0..=100.0).text("不透明度").fixed_decimals(0))
            .changed()
        {
            opacity = percent / 100.0;
        }

        let mut feather = masks.layers()[selected].shape.feather();
        if ui
            .add(
                egui::Slider::new(&mut feather, 0.0..=1.0)
                    .text("羽化")
                    .fixed_decimals(2),
            )
            .changed()
        {
            if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
                layer.shape.set_feather(feather);
            }
        }

        ui.space_group();
        ui.checkbox(&mut follow, "跟随全局提亮参数")
            .on_hover_text("取消后可以单独设置这个蒙版的提亮起点与上限");
        if follow {
            ui.metadata_label(&format!(
                "起 {:.2} · 上限 {:.2}×（与全局相同）",
                base.knee, base.max_boost
            ));
        } else {
            if ui
                .add(
                    egui::Slider::new(&mut boost, 1.0..=8.0)
                        .text("提亮上限")
                        .suffix("×")
                        .fixed_decimals(1),
                )
                .changed()
            {}
            if ui
                .add(
                    egui::Slider::new(&mut knee, 0.0..=0.95)
                        .text("提亮起点")
                        .fixed_decimals(2),
                )
                .changed()
            {}
        }

        if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
            layer.combine = combine;
            layer.invert = invert;
            layer.opacity = opacity.clamp(0.0, 1.0);
            layer.follow_base = follow;
            if !follow {
                layer.knee = knee.clamp(0.0, 0.999);
                layer.max_boost = boost.clamp(1.0, 16.0);
            }
        }

        // Shape-specific controls.
        match &masks.layers()[selected].shape.clone() {
            MaskShape::Luminance { min, max, softness } => {
                let mut low = *min;
                let mut high = *max;
                let mut soft = *softness;
                let mut dirty = false;
                dirty |= ui
                    .add(egui::Slider::new(&mut low, 0.0..=1.0).text("亮度下限").fixed_decimals(2))
                    .changed();
                dirty |= ui
                    .add(egui::Slider::new(&mut high, 0.0..=1.0).text("亮度上限").fixed_decimals(2))
                    .changed();
                dirty |= ui
                    .add(egui::Slider::new(&mut soft, 0.0..=1.0).text("过渡").fixed_decimals(2))
                    .changed();
                if dirty {
                    if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
                        layer.shape = MaskShape::Luminance {
                            min: low.min(high),
                            max: high.max(low),
                            softness: soft,
                        };
                    }
                }
            }
            MaskShape::Color {
                hue,
                hue_tolerance,
                saturation_min,
                softness,
            } => {
                let mut h = *hue;
                let mut tolerance = *hue_tolerance;
                let mut saturation = *saturation_min;
                let mut soft = *softness;
                let mut dirty = false;
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut h, 0.0..=1.0)
                            .text("色相")
                            .fixed_decimals(3),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut tolerance, 0.005..=0.5)
                            .text("容差")
                            .fixed_decimals(3),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut saturation, 0.0..=0.95)
                            .text("最低饱和")
                            .fixed_decimals(2),
                    )
                    .changed();
                dirty |= ui
                    .add(
                        egui::Slider::new(&mut soft, 0.0..=1.0)
                            .text("过渡")
                            .fixed_decimals(2),
                    )
                    .changed();
                if dirty {
                    if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
                        layer.shape = MaskShape::Color {
                            hue: h,
                            hue_tolerance: tolerance,
                            saturation_min: saturation,
                            softness: soft,
                        };
                    }
                }
                // A hue ramp, so the picked colour is visible without numbers.
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
                for step in 0..48 {
                    let t = step as f32 / 47.0;
                    let t_next = (step + 1) as f32 / 47.0;
                    let (r, g, b) = hsl_to_rgb(t);
                    let color = egui::Color32::from_rgb(r, g, b);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(rect.min.x + rect.width() * t, rect.min.y),
                            egui::pos2(rect.min.x + rect.width() * t_next, rect.max.y),
                        ),
                        0.0,
                        color,
                    );
                }
                let marker = rect.min.x + rect.width() * hue.clamp(0.0, 1.0);
                ui.painter().line_segment(
                    [
                        egui::pos2(marker, rect.min.y - 2.0),
                        egui::pos2(marker, rect.max.y + 2.0),
                    ],
                    egui::Stroke::new(2.0, egui::Color32::WHITE),
                );
            }
            MaskShape::Brush(brush) => {
                ui.metadata_label(&format!("{} 笔", brush.strokes.len()));
                let mut hardness = editor.brush_hardness;
                let mut flow = editor.brush_flow;
                let mut radius = editor.brush_radius;
                ui.add(
                    egui::Slider::new(&mut radius, 0.005..=0.5)
                        .text("笔刷大小")
                        .fixed_decimals(3),
                );
                ui.add(
                    egui::Slider::new(&mut hardness, 0.0..=1.0)
                        .text("硬度")
                        .fixed_decimals(2),
                );
                ui.add(
                    egui::Slider::new(&mut flow, 0.05..=1.0)
                        .text("不透明度")
                        .fixed_decimals(2),
                );
                editor.brush_radius = radius;
                editor.brush_hardness = hardness;
                editor.brush_flow = flow;
                ui.horizontal(|ui| {
                    ui.checkbox(&mut editor.brush_erase, "橡皮擦");
                    if ui.ghost_button("撤销一笔").clicked() {
                        if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
                            if let MaskShape::Brush(brush) = &mut layer.shape {
                                brush.undo();
                            }
                        }
                    }
                    if ui.ghost_button("清空").clicked() {
                        if let Some(layer) = Arc::make_mut(masks).layers_mut().get_mut(selected) {
                            if let MaskShape::Brush(brush) = &mut layer.shape {
                                brush.clear();
                            }
                        }
                    }
                });
            }
            MaskShape::Linear { .. } | MaskShape::Radial { .. } => {
                ui.metadata_label("在预览图上拖动可重新摆放；滚轮调羽化。");
            }
        }
    });

    masks.encode() != before
}

/// Fill the gap between two pointer samples so a fast flick still draws a
/// continuous stroke. Written as a free function because it depends only on the
/// brush radius, not on the rest of the editor.
fn interpolate(from: [f32; 2], to: [f32; 2], radius: f32, out: &mut Vec<[f32; 2]>) {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    let distance = (dx * dx + dy * dy).sqrt();
    let step = (radius * 0.35).max(0.002);
    if distance <= step {
        return;
    }
    let steps = (distance / step).ceil() as usize;
    for step_index in 1..steps.min(64) {
        let t = step_index as f32 / steps as f32;
        out.push([from[0] + dx * t, from[1] + dy * t]);
    }
}

fn hsl_to_rgb(hue: f32) -> (u8, u8, u8) {
    // Full saturation and value: what the ramp needs is recognisable hues, not
    // a faithful conversion of any particular pixel.
    let hue = hue.rem_euclid(1.0) * 6.0;
    let sector = (hue.floor() as i32) % 6;
    let fraction = hue - hue.floor();
    let falling = ((1.0 - fraction) * 255.0).round() as u8;
    let rising = (fraction * 255.0).round() as u8;
    match sector {
        0 => (255, rising, 0),
        1 => (falling, 255, 0),
        2 => (0, 255, rising),
        3 => (0, falling, 255),
        4 => (rising, 0, 255),
        _ => (255, 0, falling),
    }
}

/// Timed debounce for the full-resolution rebuild.
pub struct RebuildTimer {
    pending: Option<Instant>,
    delay: Duration,
}

impl Default for RebuildTimer {
    fn default() -> Self {
        Self {
            pending: None,
            delay: Duration::from_millis(0),
        }
    }
}

impl RebuildTimer {
    /// Ask for a rebuild once the gesture settles.
    pub fn arm(&mut self, after: Duration) {
        self.pending = Some(Instant::now() + after);
        self.delay = after;
    }

    /// `true` when the caller should rebuild now.
    pub fn ready(&mut self) -> bool {
        match self.pending {
            Some(deadline) if Instant::now() >= deadline => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }

    /// How long until the pending rebuild fires, for `request_repaint_after`.
    pub fn remaining(&self) -> Option<Duration> {
        let deadline = self.pending?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }
}
