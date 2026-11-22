//! 安卓端的蒙版编辑状态：工具、覆盖层纹理、以及「拖动中还没提交」的草稿图层。
//!
//! 桌面版把整套交互写在 `editor.rs` 里，依赖 modern-egui 的控件；安卓这份
//! 只有 Material 3 自绘控件，所以这里只保留状态与几何，交互放在 `lib.rs`
//! 的 `mask_card` 里，通过 `Msg` 派发回 `handle` 落进 `Settings::masks`。
//!
//! 拖动过程中不直接改蒙版栈——`Component::render` 拿的是 `&self`，而蒙版藏在
//! `Arc<MaskStack>` 里。于是把正在拖的形状放进 [`MaskUi::active`]，叠加层用
//! 「栈 + 草稿层」临时合成一张图；手指抬起时再把草稿层交回去提交。

use std::sync::Arc;

use jpg2ultrahdr::{
    BrushMask, BrushStroke, MaskCombine, MaskLayer, MaskShape, MaskStack, PreviewFrame, ToneParams,
    rgb_to_hsl,
};

/// 五类蒙版工具，顺序与面板上的按钮一致。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Linear,
    Radial,
    Brush,
    Luminance,
    Color,
}

impl Tool {
    pub const ALL: [Tool; 5] = [
        Tool::Linear,
        Tool::Radial,
        Tool::Brush,
        Tool::Luminance,
        Tool::Color,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Linear => "线性",
            Tool::Radial => "径向",
            Tool::Brush => "画笔",
            Tool::Luminance => "亮度",
            Tool::Color => "颜色",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Tool::Linear => "在图上从一个方向拖到另一个方向，起点处不生效、终点处全生效。",
            Tool::Radial => "从中心往外拖，拖出一个椭圆；松手后在面板里还能调羽化。",
            Tool::Brush => "手指涂抹，画过的地方按笔刷强度提亮；两指捏合可以改笔刷大小。",
            Tool::Luminance => "点一下画面，按那个位置的亮度自动取一段范围，再在面板里微调。",
            Tool::Color => "点一下画面，取那个位置的颜色做范围，再在面板里微调宽容度。",
        }
    }

    pub fn index(self) -> u8 {
        match self {
            Tool::Linear => 0,
            Tool::Radial => 1,
            Tool::Brush => 2,
            Tool::Luminance => 3,
            Tool::Color => 4,
        }
    }

    pub fn from_index(value: u8) -> Self {
        match value {
            1 => Tool::Radial,
            2 => Tool::Brush,
            3 => Tool::Luminance,
            4 => Tool::Color,
            _ => Tool::Linear,
        }
    }

    /// 这个工具是不是靠拖拽画出形状（否则点一下就用采样值建层）。
    pub fn is_drag(self) -> bool {
        matches!(self, Tool::Linear | Tool::Radial | Tool::Brush)
    }
}

/// 叠加层显示什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overlay {
    All,
    Selected,
    None,
}

impl Overlay {
    pub fn label(self) -> &'static str {
        match self {
            Overlay::All => "全部蒙版",
            Overlay::Selected => "当前蒙版",
            Overlay::None => "原图",
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Overlay::All => 0,
            Overlay::Selected => 1,
            Overlay::None => 2,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Overlay::Selected,
            2 => Overlay::None,
            _ => Overlay::All,
        }
    }
}

/// 编辑器状态。`render` 只有 `&self`，所以整体放在 `RefCell` 里。
pub struct MaskUi {
    pub tool: Tool,
    pub overlay: Overlay,
    /// 是否显示「如果现在就导出，HDR 看起来是什么样」。
    pub simulate: bool,
    pub exposure: f32,
    pub brush_radius: f32,
    pub brush_hardness: f32,
    pub brush_flow: f32,
    pub brush_erase: bool,

    /// 正在改名的蒙版：`(层号, 编辑中的文本)`。
    ///
    /// 文本不能塞进 `Msg`（那条枚举要 `Copy`），所以草稿放在这里，
    /// 提交时只派发一个无参消息。
    pub rename: Option<(usize, String)>,

    /// 正在拖的草稿层。存在时叠加层会额外合成它。
    active: Option<Box<MaskLayer>>,
    /// 松手后等待提交的草稿层，由 `Msg::CommitDraft` 取走。
    pending: Option<Box<MaskLayer>>,
    /// 拖拽起点（归一化坐标）。
    anchor: Option<[f32; 2]>,

    texture: Option<egui::TextureHandle>,
    texture_key: String,
}

impl Default for MaskUi {
    fn default() -> Self {
        Self {
            tool: Tool::Linear,
            overlay: Overlay::All,
            simulate: false,
            exposure: 1.0,
            brush_radius: 0.06,
            brush_hardness: 0.85,
            brush_flow: 1.0,
            brush_erase: false,
            rename: None,
            active: None,
            pending: None,
            anchor: None,
            texture: None,
            texture_key: String::new(),
        }
    }
}

impl MaskUi {
    /// 换了照片之后清掉草稿与缓存的纹理。
    pub fn reset(&mut self) {
        self.active = None;
        self.pending = None;
        self.anchor = None;
        self.rename = None;
        self.texture = None;
        self.texture_key.clear();
    }

    pub fn is_dragging(&self) -> bool {
        self.active.is_some()
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn take_pending(&mut self) -> Option<MaskLayer> {
        self.pending.take().map(|boxed| *boxed)
    }

    pub fn cancel_drag(&mut self) {
        self.active = None;
        self.anchor = None;
    }

    /// 手势开始：按工具造一个草稿层。
    ///
    /// `next_index` 只用来给新层起名字（「线性 1」「画笔 2」这样的序号）。
    pub fn begin(
        &mut self,
        point: [f32; 2],
        level: f32,
        rgb: [u8; 3],
        tone: ToneParams,
        next_index: usize,
    ) {
        let shape = match self.tool {
            Tool::Linear => MaskShape::Linear {
                start: point,
                end: point,
                // 羽化 1.0：拖动初期形状还退化，给一段软边更接近 LrC。
                feather: 1.0,
            },
            Tool::Radial => MaskShape::Radial {
                center: point,
                radius: [0.0001, 0.0001],
                rotation: 0.0,
                feather: 0.5,
            },
            Tool::Brush => MaskShape::Brush(BrushMask::default()),
            Tool::Luminance => {
                let band = 0.12f32;
                MaskShape::Luminance {
                    min: (level - band).clamp(0.0, 1.0),
                    max: (level + band).clamp(0.0, 1.0),
                    softness: 0.35,
                }
            }
            Tool::Color => {
                let (hue, saturation, _) = rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
                MaskShape::Color {
                    hue,
                    hue_tolerance: 0.08,
                    saturation_min: (saturation * 0.4).clamp(0.0, 0.5),
                    softness: 0.3,
                }
            }
        };
        let name = format!("{} {next_index}", self.tool.label());
        let mut layer = MaskLayer::new(name, shape);
        // 新蒙版直接用一条比全局更激进的曲线：起点更低。用户画一笔就是想要
        // 「这里再亮一点」，如果还挂着 follow_base，画完什么都没发生（蒙版
        // 权重有了，但 gain 仍等于全局曲线），会被当成 bug。
        //
        // 上限必须**不超过**全局：MaskStack::capacity() 取所有层与全局的最大
        // 值，一旦某层把它抬高，增益图的对数刻度 log_max 就变了，整张图的
        // 每个字节都会重编码（实测 89.6% 的行都变），而用户只想改画过的地方。
        // 靠「起点低一点」就足够让它更亮。面板里的「跟随全局提亮」开关可以把
        // 它随时拉回全局。
        layer.follow_base = false;
        layer.max_boost = tone.max_boost;
        layer.knee = (tone.knee - 0.15).clamp(0.0, 0.9);
        self.anchor = Some(point);
        self.active = Some(Box::new(layer));
    }

    /// 拖到新位置：线性改终点，径向改长短半径，画笔补一个采样点。
    pub fn drag_to(&mut self, point: [f32; 2], from: [f32; 2]) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let Some(layer) = self.active.as_mut() else {
            return;
        };
        match &mut layer.shape {
            MaskShape::Linear { start, end, .. } => {
                *start = anchor;
                *end = point;
            }
            MaskShape::Radial { center, radius, .. } => {
                *center = anchor;
                radius[0] = (point[0] - anchor[0]).abs().max(0.005);
                radius[1] = (point[1] - anchor[1]).abs().max(0.005);
            }
            MaskShape::Brush(mask) => {
                if mask.strokes.is_empty() {
                    let mut stroke = BrushStroke::new(
                        self.brush_radius,
                        self.brush_hardness,
                        self.brush_flow,
                        self.brush_erase,
                        from,
                    );
                    stroke.push(point);
                    mask.push(stroke);
                } else {
                    // 快速滑动时一帧只来一个位置，中间补点避免断成一串孤立圆。
                    let mut extra = Vec::new();
                    interpolate(from, point, self.brush_radius, &mut extra);
                    for sample in extra {
                        if let Some(stroke) = mask.strokes.last_mut() {
                            stroke.push(sample);
                        }
                    }
                    if let Some(stroke) = mask.strokes.last_mut() {
                        stroke.push(point);
                    }
                }
            }
            _ => {}
        }
    }

    /// 松手：把草稿层挪到待提交槽。
    ///
    /// 画笔草稿如果没画到任何点（纯点击），会顺手补一个点，这样「点一下」
    /// 也能落一个圆点，和桌面版一致。
    pub fn finish(&mut self, point: [f32; 2]) {
        let Some(mut layer) = self.active.take() else {
            return;
        };
        self.anchor = None;
        if let MaskShape::Brush(mask) = &mut layer.shape {
            if mask.is_empty() {
                let mut stroke = BrushStroke::new(
                    self.brush_radius,
                    self.brush_hardness,
                    self.brush_flow,
                    self.brush_erase,
                    point,
                );
                stroke.push([point[0] + 1e-3, point[1]]);
                mask.push(stroke);
            }
        }
        self.pending = Some(layer);
    }

    /// 双指捏合：画笔改笔刷大小，渐变类改羽化。
    ///
    /// 返回 `true` 表示这次捏合被用在选中的蒙版上（需要提交到栈里）。
    pub fn pinch(&mut self, factor: f32, masks: &mut Arc<MaskStack>, selected: usize) -> bool {
        if !(factor.is_finite() && factor > 0.0) || (factor - 1.0).abs() < 0.01 {
            return false;
        }
        if self.tool == Tool::Brush {
            self.brush_radius = (self.brush_radius * factor).clamp(0.005, 0.5);
            return false;
        }
        let Some(layer) = masks.layers().get(selected) else {
            return false;
        };
        let current = layer.shape.feather();
        let next = (current * factor).clamp(0.0, 1.0);
        if (next - current).abs() < 1e-4 {
            return false;
        }
        Arc::make_mut(masks).layers_mut()[selected]
            .shape
            .set_feather(next);
        true
    }

    /// 「蒙版栈 + 正在拖的草稿层」，用来算叠加层与模拟预览。
    pub fn composed(&self, masks: &MaskStack) -> MaskStack {
        let mut stack = masks.clone();
        if let Some(layer) = &self.active {
            stack.layers_mut().push((**layer).clone());
        }
        stack
    }

    fn key(
        &self,
        frame: &PreviewFrame,
        masks: &MaskStack,
        tone: ToneParams,
        selected: usize,
    ) -> String {
        let mut key = format!(
            "{}x{}|{}|{}|{}|{}|{:.3}|{:.3}|{:.3}",
            frame.width,
            frame.height,
            masks.encode(),
            self.overlay.index(),
            selected,
            self.simulate,
            tone.knee,
            tone.max_boost,
            self.exposure,
        );
        if let Some(layer) = &self.active {
            key.push_str("|draft:");
            let mut one = MaskStack::default();
            one.layers_mut().push((**layer).clone());
            key.push_str(&one.encode());
        }
        key
    }

    /// 按当前模式重算叠加层 / 模拟图，变化时才重新上传纹理。
    pub fn ensure_texture(
        &mut self,
        ctx: &egui::Context,
        frame: &PreviewFrame,
        masks: &MaskStack,
        tone: ToneParams,
        selected: usize,
    ) {
        let key = self.key(frame, masks, tone, selected);
        if self.texture.is_some() && self.texture_key == key {
            return;
        }
        let stack = self.composed(masks);
        let rgba = if self.simulate {
            frame.simulated_rgba(&stack, tone, self.exposure)
        } else if self.overlay == Overlay::None {
            // 只看原图：不加任何着色，但仍然受草稿影响不了，所以直接复制。
            let mut out = Vec::with_capacity(frame.rgb.len() / 3 * 4);
            for pixel in frame.rgb.chunks_exact(3) {
                out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            out
        } else if self.overlay == Overlay::Selected {
            // 草稿层永远追加在末尾；只显示当前层时看的就是它。
            let index = if self.active.is_some() {
                stack.len().saturating_sub(1)
            } else {
                selected.min(stack.len().saturating_sub(1))
            };
            frame.overlay_one_rgba(&stack, index)
        } else {
            frame.overlay_rgba(&stack)
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &rgba,
        );
        self.texture = Some(ctx.load_texture(
            "mask_overlay",
            image,
            egui::TextureOptions::LINEAR,
        ));
        self.texture_key = key;
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture.as_ref().map(|handle| handle.id())
    }
}

/// 快速滑动时在两点之间补采样点。
///
/// `pointer` 一帧只报一个位置，手指快的时候两次采样能差出好几个笔刷宽度，
/// 直接连过去会得到一条「毛刺」边而不是圆头笔画。
pub fn interpolate(from: [f32; 2], to: [f32; 2], radius: f32, out: &mut Vec<[f32; 2]>) {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    let distance = (dx * dx + dy * dy).sqrt();
    let step = (radius * 0.35).max(0.002);
    if distance <= step {
        return;
    }
    let steps = ((distance / step).ceil() as usize).min(64);
    for index in 1..steps {
        let t = index as f32 / steps as f32;
        out.push([from[0] + dx * t, from[1] + dy * t]);
    }
}

/// 组合方式的短标签，给列表里的循环切换按钮用。
pub fn combine_label(combine: MaskCombine) -> &'static str {
    combine.label()
}

/// 色相条上的一个颜色。全饱和全明度，只用来让人看出选的是哪一段。
pub fn hue_rgb(hue: f32) -> (u8, u8, u8) {
    let hue = hue.rem_euclid(1.0) * 6.0;
    let sector = hue.floor() as i32 % 6;
    let falling = 1.0 - (hue - hue.floor());
    let rising = hue - hue.floor();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    match sector {
        0 => (255, channel(rising), 0),
        1 => (channel(falling), 255, 0),
        2 => (0, 255, channel(rising)),
        3 => (0, channel(falling), 255),
        4 => (channel(rising), 0, 255),
        _ => (255, 0, channel(falling)),
    }
}
