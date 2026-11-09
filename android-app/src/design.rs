//! Material Design 3 的最小实现。
//!
//! egui-android-framework 自带的 `MaterialTheme` 只提供了 12 个基础色槽，
//! 而 M3 的层次感依赖 surface container 五级色阶、primary/secondary container
//! 等扩展槽位。这里用基础色按 M3 的色调规则推导出完整色板，再手写出
//! TopAppBar / FilledButton / Card / Slider / Switch / SegmentedButton /
//! LinearProgress 这些 M3 组件，避免框架控件在中文与触摸屏上的排版问题。

use egui::{
    Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
    epaint::CornerRadius,
};

use egui_android_framework::ui::theme::ColorPalette;

/// 在两个颜色之间线性插值，`t = 0` 取 `a`，`t = 1` 取 `b`。
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn alpha(color: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

/// M3 色板。字段命名遵循 Material Design 3 的 token 名。
///
/// 即使当前界面没用到全部槽位，也完整保留，这样新增组件时不必再改色板推导。
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct M3 {
    pub dark: bool,
    pub primary: Color32,
    pub on_primary: Color32,
    pub primary_container: Color32,
    pub on_primary_container: Color32,
    pub secondary: Color32,
    pub on_secondary: Color32,
    pub secondary_container: Color32,
    pub on_secondary_container: Color32,
    pub surface: Color32,
    pub on_surface: Color32,
    pub surface_container_lowest: Color32,
    pub surface_container_low: Color32,
    pub surface_container: Color32,
    pub surface_container_high: Color32,
    pub surface_container_highest: Color32,
    pub surface_variant: Color32,
    pub on_surface_variant: Color32,
    pub outline: Color32,
    pub outline_variant: Color32,
    pub error: Color32,
    pub on_error: Color32,
}

impl M3 {
    /// 从框架的主题色板推导出完整的 M3 色板。
    pub fn new(colors: &ColorPalette, dark: bool) -> Self {
        let white = Color32::WHITE;
        let black = Color32::BLACK;
        let surface = colors.surface;
        let on_surface = colors.on_surface;
        let primary = colors.primary;
        let secondary = colors.secondary;

        // 深色主题靠「往白色方向加一点」抬升容器层级，浅色则往文字色方向压暗。
        let (lowest, low, container, high, highest) = if dark {
            (
                mix(surface, black, 0.16),
                mix(surface, white, 0.03),
                mix(surface, white, 0.06),
                mix(surface, white, 0.10),
                mix(surface, white, 0.14),
            )
        } else {
            (
                white,
                mix(surface, on_surface, 0.025),
                mix(surface, on_surface, 0.055),
                mix(surface, on_surface, 0.085),
                mix(surface, on_surface, 0.115),
            )
        };

        let (primary_container, on_primary_container) = if dark {
            (mix(primary, colors.background, 0.62), mix(primary, white, 0.55))
        } else {
            (mix(primary, white, 0.86), mix(primary, black, 0.62))
        };

        let (secondary_container, on_secondary_container) = if dark {
            (mix(secondary, colors.background, 0.55), mix(secondary, white, 0.55))
        } else {
            // 浅色下用主色淡染，和 surface_container_high 的灰色轨道拉开区分度。
            (mix(primary, white, 0.78), mix(primary, black, 0.55))
        };

        Self {
            dark,
            primary,
            on_primary: colors.on_primary,
            primary_container,
            on_primary_container,
            secondary,
            on_secondary: colors.on_secondary,
            secondary_container,
            on_secondary_container,
            surface,
            on_surface,
            surface_container_lowest: lowest,
            surface_container_low: low,
            surface_container: container,
            surface_container_high: high,
            surface_container_highest: highest,
            surface_variant: mix(surface, on_surface, if dark { 0.14 } else { 0.09 }),
            on_surface_variant: mix(on_surface, surface, if dark { 0.22 } else { 0.30 }),
            outline: colors.outline,
            outline_variant: colors.outline_variant,
            error: colors.error,
            on_error: colors.on_error,
        }
    }
}

/// 一条拖动被锁定到的方向。触发阈值后不再改变，直到手指抬起。
#[derive(Clone, Copy, PartialEq)]
enum DragAxis {
    Horizontal,
    Vertical,
}

/// 按钮的视觉档次，对应 M3 的四种按钮。
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
pub enum ButtonKind {
    /// 高强调：主色实心。
    Filled,
    /// 中强调：次要容器色。
    Tonal,
    /// 低强调：描边。
    Outlined,
    /// 最低强调：纯文字。
    Text,
}

impl ButtonKind {
    fn colors(self, m: &M3, enabled: bool) -> (Color32, Color32, Option<Color32>) {
        if !enabled {
            let bg = match self {
                ButtonKind::Filled => mix(m.on_surface, m.surface, 0.88),
                ButtonKind::Tonal => mix(m.on_surface, m.surface, 0.92),
                ButtonKind::Outlined | ButtonKind::Text => Color32::TRANSPARENT,
            };
            let fg = mix(m.on_surface, m.surface, 0.62);
            let border = match self {
                ButtonKind::Outlined => Some(mix(m.outline, m.surface, 0.55)),
                _ => None,
            };
            return (bg, fg, border);
        }
        match self {
            ButtonKind::Filled => (m.primary, m.on_primary, None),
            ButtonKind::Tonal => (m.secondary_container, m.on_secondary_container, None),
            ButtonKind::Outlined => (Color32::TRANSPARENT, m.primary, Some(m.outline)),
            ButtonKind::Text => (Color32::TRANSPARENT, m.primary, None),
        }
    }
}

/// M3 按钮。返回是否被点击。
pub fn button(ui: &mut Ui, m: &M3, label: impl Into<String>, kind: ButtonKind, enabled: bool) -> bool {
    let label = label.into();
    let height = 52.0;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let (bg, fg, border) = kind.colors(m, enabled);

    let painter = ui.painter();
    if bg != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::same(16), bg);
    }
    if let Some(border_color) = border {
        painter.rect_stroke(
            rect,
            CornerRadius::same(16),
            Stroke::new(1.0, border_color),
            StrokeKind::Inside,
        );
    }
    // 状态层：悬停 8%、按下 12%，这是 M3 的标准不透明度。
    if enabled {
        if response.is_pointer_button_down_on() {
            painter.rect_filled(rect, CornerRadius::same(16), alpha(fg, 31));
        } else if response.hovered() {
            painter.rect_filled(rect, CornerRadius::same(16), alpha(fg, 20));
        }
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        &label,
        FontId::proportional(15.0),
        fg,
    );

    enabled && response.clicked()
}

/// M3 卡片：圆角 20、填充 surface container，可选描边。
pub fn card<R>(ui: &mut Ui, m: &M3, elevated: bool, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = if elevated {
        m.surface_container_high
    } else {
        m.surface_container_low
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, m.outline_variant))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(16, 16));

    frame.show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        add(ui)
    })
    .inner
}

/// M3 小标题：`titleMedium` 风格。
pub fn section_title(ui: &mut Ui, m: &M3, title: &str, trailing: Option<&str>) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title)
                .size(16.0)
                .color(m.on_surface)
                .strong(),
        );
        if let Some(trailing) = trailing {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(trailing)
                        .size(13.0)
                        .color(m.on_surface_variant),
                );
            });
        }
    });
    ui.add_space(10.0);
}

/// M3 滑块：上方是标签 + 当前值，下方是可拖动的轨道。
/// 返回 `true` 表示值发生了变化。
pub fn slider(
    ui: &mut Ui,
    m: &M3,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    decimals: usize,
    suffix: &str,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(14.0).color(m.on_surface));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!("{value:.*}{suffix}", decimals))
                    .size(14.0)
                    .color(m.primary)
                    .strong(),
            );
        });
    });

    // 只用 click 感知：竖直拖动交给外层的 ScrollArea 滚动页面，
    // 否则手指从滑块上滑下去会变成「调数值」，页面滚不动。
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());

    let (min, max) = (*range.start(), *range.end());
    let span = (max - min).max(f32::EPSILON);
    let thumb_radius = 10.0;
    let travel = (rect.width() - thumb_radius * 2.0).max(1.0);

    // 手势判定：按下后第一次超过阈值的位移决定这条手势是「调数值」还是「滚页面」，
    // 之后整条手势都锁定这个方向。
    //
    // 为什么不能看单帧 `pointer.delta()`：拖动时每帧的位移只有几像素，横竖两个
    // 分量大小随机，判定会来回翻转，于是横向拖动动不动就被判成竖直而失效。
    // 改用 `press_origin`（本次按下点，整条手势内不变）比较累计位移，判定就稳定了。
    //
    // 为什么不能看 `response.is_pointer_button_down_on()`：它的语义是
    // 「egui 仍认为这次按下的目标是我」（`potential_click_id == Some(id)`），
    // 而本控件只感知 click，手指一移动超过拖动阈值，egui 就把 potential_click_id
    // 清掉、该标志立刻变 false —— 正好是我们最需要它保持 true 的时刻。
    // 所以这里直接读原始指针状态。
    let axis_id = response.id.with("drag_axis");
    let mut axis: Option<DragAxis> = ui.ctx().data(|d| d.get_temp(axis_id));

    let (origin, latest, down) = ui.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
        )
    });
    // 只认「本次按下点落在滑块轨道上」的手势，避免别处的手势串进这个控件。
    let pressed_here = down
        && matches!((origin, latest), (Some(o), Some(_)) if rect.contains(o));

    if pressed_here {
        if axis.is_none() {
            if let (Some(o), Some(cur)) = (origin, latest) {
                let (dx, dy) = ((cur.x - o.x).abs(), (cur.y - o.y).abs());
                // 10 像素触摸阈值：小于它视为「还没想好」，避免抖动误判。
                if dx.max(dy) > 10.0 {
                    axis = Some(if dx > dy {
                        DragAxis::Horizontal
                    } else {
                        DragAxis::Vertical
                    });
                }
            }
        }
    } else {
        axis = None;
    }
    ui.ctx().data_mut(|d| d.insert_temp(axis_id, axis));

    // 轻点轨道直接跳到该位置；拖动只在锁定为横向时生效。
    if response.clicked() || (axis == Some(DragAxis::Horizontal) && pressed_here) {
        if let Some(pos) = if response.clicked() {
            response.interact_pointer_pos()
        } else {
            latest
        } {
            let next = ((pos.x - rect.left() - thumb_radius) / travel).clamp(0.0, 1.0);
            let candidate = min + next * span;
            if (candidate - *value).abs() > span * 0.0005 {
                *value = candidate;
                changed = true;
            }
        }
    }

    let t = ((*value - min) / span).clamp(0.0, 1.0);
    let center_y = rect.center().y;
    let track_h = 8.0;
    let left = rect.left() + thumb_radius;
    let right = rect.right() - thumb_radius;

    let painter = ui.painter();
    let track = Rect::from_min_max(
        egui::pos2(left, center_y - track_h / 2.0),
        egui::pos2(right, center_y + track_h / 2.0),
    );
    painter.rect_filled(track, CornerRadius::same(4), m.surface_container_highest);
    if t > 0.0 {
        let active = Rect::from_min_max(
            track.min,
            egui::pos2(left + (right - left) * t, track.max.y),
        );
        painter.rect_filled(active, CornerRadius::same(4), m.primary);
    }

    let thumb_x = left + (right - left) * t;
    if response.is_pointer_button_down_on() {
        painter.circle_filled(
            egui::pos2(thumb_x, center_y),
            thumb_radius + 8.0,
            alpha(m.primary, 40),
        );
    } else if response.hovered() {
        painter.circle_filled(
            egui::pos2(thumb_x, center_y),
            thumb_radius + 6.0,
            alpha(m.primary, 26),
        );
    }
    painter.circle_filled(egui::pos2(thumb_x, center_y), thumb_radius, m.primary);
    // 手柄中心的白色小点，让滑块在深色/浅色下都一眼可见。
    painter.circle_filled(egui::pos2(thumb_x, center_y), 3.5, m.surface);

    ui.add_space(4.0);
    changed
}

/// M3 开关行：左侧两行文字，右侧开关。返回 `true` 表示被切换。
pub fn switch(
    ui: &mut Ui,
    m: &M3,
    id: &str,
    title: &str,
    subtitle: Option<&str>,
    value: &mut bool,
) -> bool {
    let height = if subtitle.is_some() { 56.0 } else { 44.0 };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());

    let track_w = 52.0;
    let track_h = 32.0;
    let track = Rect::from_center_size(
        egui::pos2(rect.right() - track_w / 2.0, rect.center().y),
        Vec2::new(track_w, track_h),
    );

    let anim = ui
        .ctx()
        .animate_bool_with_time(ui.id().with(id), *value, 0.14);

    let text_right = track.left() - 12.0;
    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left(), rect.center().y - if subtitle.is_some() { 11.0 } else { 0.0 }),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.5),
        m.on_surface,
    );
    if let Some(subtitle) = subtitle {
        painter.text(
            egui::pos2(rect.left(), rect.center().y + 11.0),
            Align2::LEFT_CENTER,
            subtitle,
            FontId::proportional(12.5),
            m.on_surface_variant,
        );
    }
    let _ = text_right;

    let track_color = mix(m.surface_container_highest, m.primary, anim);
    painter.rect_filled(track, CornerRadius::same(16), track_color);
    if anim < 0.5 {
        painter.rect_stroke(
            track,
            CornerRadius::same(16),
            Stroke::new(2.0, m.outline),
            StrokeKind::Inside,
        );
    }
    let knob_x = track.left() + 16.0 + (track_w - 32.0) * anim;
    let knob_color = mix(m.outline, m.on_primary, anim);
    let knob_radius = 8.0 + 2.0 * anim;
    painter.circle_filled(egui::pos2(knob_x, track.center().y), knob_radius, knob_color);

    if response.clicked() {
        *value = !*value;
        return true;
    }
    false
}

/// M3 分段按钮（segmented button）。返回用户新选中的值。
pub fn segmented(
    ui: &mut Ui,
    m: &M3,
    id: &str,
    current: u32,
    options: &[(u32, &str)],
) -> Option<u32> {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 48.0), Sense::hover());

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(24), m.surface_container_high);
    painter.rect_stroke(
        rect,
        CornerRadius::same(24),
        Stroke::new(1.0, m.outline_variant),
        StrokeKind::Inside,
    );

    let count = options.len() as f32;
    let seg_width = (width - 8.0) / count;
    let mut chosen = None;

    for (index, (value, label)) in options.iter().enumerate() {
        let seg = Rect::from_min_size(
            egui::pos2(rect.left() + 4.0 + index as f32 * seg_width, rect.top() + 4.0),
            Vec2::new(seg_width, rect.height() - 8.0),
        );
        let response = ui.interact(seg, ui.id().with((id, *value)), Sense::click());
        let selected = *value == current;

        if selected {
            painter.rect_filled(seg, CornerRadius::same(20), m.secondary_container);
        } else if response.is_pointer_button_down_on() {
            painter.rect_filled(seg, CornerRadius::same(20), alpha(m.on_surface, 24));
        } else if response.hovered() {
            painter.rect_filled(seg, CornerRadius::same(20), alpha(m.on_surface, 14));
        }

        painter.text(
            seg.center(),
            Align2::CENTER_CENTER,
            *label,
            FontId::proportional(13.5),
            if selected {
                m.on_secondary_container
            } else {
                m.on_surface_variant
            },
        );

        if response.clicked() && !selected {
            chosen = Some(*value);
        }
    }

    chosen
}

/// M3 线性进度条。
pub fn linear_progress(ui: &mut Ui, m: &M3, fraction: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(2), m.surface_container_highest);
    let t = fraction.clamp(0.0, 1.0);
    if t > 0.0 {
        let filled = Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + rect.width() * t, rect.bottom()),
        );
        painter.rect_filled(filled, CornerRadius::same(2), m.primary);
    }
}

/// M3 圆角标签（chip），用于状态。
#[allow(dead_code)]
pub fn chip(ui: &mut Ui, text: &str, bg: Color32, fg: Color32) {
    let font = FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), fg);
    let size = Vec2::new(galley.size().x + 18.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
}

/// 顶部应用栏：标题 + 副标题 + 主题切换按钮。
/// 返回 `true` 表示用户点了主题按钮。
pub fn top_bar(ui: &mut Ui, m: &M3, title: &str, subtitle: &str) -> bool {
    let mut toggle = false;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 64.0), Sense::hover());

    let button = Rect::from_center_size(
        egui::pos2(rect.right() - 32.0, rect.center().y),
        Vec2::splat(48.0),
    );
    let response = ui.interact(button, ui.id().with("theme_toggle"), Sense::click());

    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left(), rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(21.0),
        m.on_surface,
    );
    painter.text(
        egui::pos2(rect.left(), rect.center().y + 13.0),
        Align2::LEFT_CENTER,
        subtitle,
        FontId::proportional(12.5),
        m.on_surface_variant,
    );

    if response.hovered() || response.is_pointer_button_down_on() {
        painter.circle_filled(button.center(), 22.0, alpha(m.on_surface, 22));
    }

    // 手绘太阳/月亮，不依赖字体里的符号（系统字体缺 ☀/☾ 会显示成方框）。
    let center = button.center();
    let r = 8.5;
    if m.dark {
        painter.circle_filled(center, r, m.primary);
        painter.circle_filled(
            egui::pos2(center.x + r * 0.62, center.y - r * 0.55),
            r * 0.92,
            m.surface,
        );
    } else {
        painter.circle_filled(center, r * 0.62, m.primary);
        for step in 0..8 {
            let angle = std::f32::consts::TAU * step as f32 / 8.0;
            let dir = Vec2::new(angle.cos(), angle.sin());
            painter.line_segment(
                [center + dir * (r * 0.95), center + dir * (r * 1.45)],
                Stroke::new(1.8, m.primary),
            );
        }
    }

    if response.clicked() {
        toggle = true;
    }
    toggle
}
