//! Android 端的「JPG → Ultra HDR」图形界面。
//!
//! 复用桌面版的转换内核（`jpg2ultrahdr` crate）：
//! 解码 → 生成增益图 → `ultrajpeg::encode` 封装成 Ultra HDR JPEG。
//!
//! 界面按 Material Design 3 规范自己实现（见 `design` 模块）：
//! 框架自带的 `MaterialTheme` 只有 12 个基础色槽，缺少 M3 的
//! surface container 色阶与容器色，控件也只有最基础的几种。
//!
//! 框架不提供文件选择与相册写入，这两件事通过 `jni_bridge` 调用
//! `EguiActivity` 里的 Kotlin 静态方法完成。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;

use egui_android_framework::prelude::*;
use egui_android_framework::runtime::{RuntimeConfig, RuntimeContext, UiNotifier};
use egui_android_framework::ui::theme::{MaterialTheme, Theme};

use jpg2ultrahdr::{
    MaskCombine, MaskShape, MaskStack, PreviewFrame, Report, Settings, ToneParams,
    convert as convert_one, output_path,
};

mod design;
mod mask_ui;
use design::{
    ButtonKind, M3, button, card, linear_progress, mix, section_title, segmented, slider, switch,
    top_bar,
};
use mask_ui::{MaskUi, Overlay, Tool, combine_label};

// ─── 全局：由 android_main 写入，供 App::create 使用 ──────────────────

/// 转换结果的落盘目录（App 私有目录，之后再由 Kotlin 写入相册）。
static OUT_DIR: OnceLock<PathBuf> = OnceLock::new();

fn out_dir() -> &'static Path {
    OUT_DIR.get_or_init(|| {
        std::env::temp_dir().join("jpg2ultrahdr-out")
    })
}

// ─── JNI 桥 ─────────────────────────────────────────────────────────

#[cfg(target_os = "android")]
mod jni_bridge {
    use std::sync::OnceLock;

    use jni::JavaVM;
    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::sys::{jobject, jstring};

    static VM: OnceLock<JavaVM> = OnceLock::new();
    static ACTIVITY: OnceLock<GlobalRef> = OnceLock::new();

    /// 记录 JavaVM 与 Activity 的全局引用。
    ///
    /// 必须在 `app` 被交给框架之前调用，之后框架会接管 `AndroidApp`。
    pub fn init(vm_ptr: *mut std::ffi::c_void, activity_ptr: *mut std::ffi::c_void) {
        let vm = match unsafe { JavaVM::from_raw(vm_ptr as *mut jni::sys::JavaVM) } {
            Ok(vm) => vm,
            Err(e) => {
                log::error!("JNI: 无法获取 JavaVM: {e}");
                return;
            }
        };

        let env = match vm.attach_current_thread_permanently() {
            Ok(env) => env,
            Err(e) => {
                log::error!("JNI: 无法附加当前线程: {e}");
                return;
            }
        };

        // Activity 的指针是一个未持有的 JNI 引用，转成全局引用才能跨线程长期使用。
        let activity = unsafe { JObject::from_raw(activity_ptr as jobject) };
        match env.new_global_ref(activity) {
            Ok(global) => {
                let _ = ACTIVITY.set(global);
                log::info!("JNI: Activity 全局引用已建立");
            }
            Err(e) => log::error!("JNI: 建立 Activity 全局引用失败: {e}"),
        }

        let _ = VM.set(vm);
    }

    /// 在任意线程上取得 JNIEnv。
    fn env() -> Option<jni::JNIEnv<'static>> {
        let vm = VM.get()?;
        match vm.attach_current_thread_permanently() {
            Ok(env) => Some(env),
            Err(e) => {
                log::error!("JNI: 附加线程失败: {e}");
                None
            }
        }
    }

    /// 调用 `EguiActivity` 上的静态方法。
    fn call_static(
        name: &str,
        sig: &str,
        args: &[JValue<'_, '_>],
    ) -> Option<jni::objects::JValueOwned<'static>> {
        let mut env = env()?;
        let activity = ACTIVITY.get()?;
        let class = match env.get_object_class(activity.as_obj()) {
            Ok(class) => class,
            Err(e) => {
                log::error!("JNI: 取 Activity 类失败: {e}");
                return None;
            }
        };
        match env.call_static_method(&class, name, sig, args) {
            Ok(value) => Some(value),
            Err(e) => {
                log::error!("JNI: 调用 {name}{sig} 失败: {e}");
                None
            }
        }
    }

    /// 拉起系统图片选择器（可多选）。
    pub fn pick_images() {
        let _ = call_static("pickImages", "()V", &[]);
    }

    /// 取走新选中的图片路径（换行分隔，取走即清空）。
    pub fn take_picked() -> Vec<std::path::PathBuf> {
        let Some(value) = call_static("takePickedPaths", "()Ljava/lang/String;", &[]) else {
            return Vec::new();
        };
        let Ok(obj) = value.l() else {
            return Vec::new();
        };
        if obj.is_null() {
            return Vec::new();
        }
        let Some(mut env) = env() else {
            return Vec::new();
        };
        let string = jni::objects::JString::from(obj);
        let text = match env.get_string(&string) {
            Ok(text) => String::from(text),
            Err(e) => {
                log::error!("JNI: 读取路径字符串失败: {e}");
                return Vec::new();
            }
        };
        text.split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(std::path::PathBuf::from)
            .collect()
    }

    /// 把若干文件写入系统相册，返回成功数量。
    pub fn save_to_gallery(paths: &[std::path::PathBuf]) -> usize {
        if paths.is_empty() {
            return 0;
        }
        let joined = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let env = match env() {
            Some(env) => env,
            None => return 0,
        };
        let string: jstring = match env.new_string(joined) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                log::error!("JNI: 创建路径字符串失败: {e}");
                return 0;
            }
        };
        let arg = unsafe { JObject::from_raw(string) };
        let value = call_static(
            "savePathsToGallery",
            "(Ljava/lang/String;)I",
            &[JValue::Object(&arg)],
        );
        let _ = env.delete_local_ref(arg);

        match value.and_then(|v| v.i().ok()) {
            Some(count) => count.max(0) as usize,
            None => 0,
        }
    }
}

// ─── 转换线程 ───────────────────────────────────────────────────────

enum Cmd {
    /// 待转换任务 + 本批使用的参数（界面上的滑块要真的生效）。
    Start(Vec<(usize, PathBuf)>, Settings),
}

enum Event {
    Started(usize),
    Done(usize, Result<Summary, String>),
    AllDone,
}

#[derive(Clone, Debug)]
struct Summary {
    text: String,
    output: PathBuf,
}

fn worker_loop(cmd_rx: mpsc::Receiver<Cmd>, evt_tx: mpsc::Sender<Event>, wake_tx: mpsc::Sender<()>) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Start(jobs, settings) => {
                let workers = concurrency(jobs.len());
                let next = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|scope| {
                    let jobs = &jobs;
                    let next = &next;
                    let settings = &settings;
                    let evt_tx = evt_tx.clone();
                    let wake_tx = &wake_tx;
                    for _ in 0..workers {
                        let evt_tx = evt_tx.clone();
                        scope.spawn(move || {
                            loop {
                                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if i >= jobs.len() {
                                    break;
                                }
                                let (index, src) = &jobs[i];
                                let _ = evt_tx.send(Event::Started(*index));
                                let _ = wake_tx.send(());

                                let result = run_one(src, settings);
                                let _ = evt_tx.send(Event::Done(*index, result));
                                let _ = wake_tx.send(());
                            }
                        });
                    }
                });
                let _ = evt_tx.send(Event::AllDone);
                let _ = wake_tx.send(());
            }
        }
    }
}

fn run_one(src: &Path, settings: &Settings) -> Result<Summary, String> {
    log::info!("run_one: 开始 {src:?}");
    std::fs::create_dir_all(out_dir()).map_err(|e| format!("创建输出目录失败：{e}"))?;
    let dst = output_path(src, Some(out_dir()), settings.format);
    log::info!("run_one: 输出到 {dst:?}");

    let report: Report =
        convert_one(src, &dst, settings).map_err(|e| format!("{e:#}"))?;
    log::info!("run_one: 完成 {dst:?}");

    let text = format!(
        "{}×{} · 增益图 {}×{} · 峰值 {:.1}× · {:.2} MB",
        report.width,
        report.height,
        report.gain_width,
        report.gain_height,
        report.peak_boost,
        report.bytes as f64 / 1_048_576.0
    );
    Ok(Summary { text, output: dst })
}

/// 手机上并行 2~4 个任务比较合适：内存有限，20MP 一张要 ~150 MB。
fn concurrency(jobs: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    cores.saturating_sub(1).clamp(1, 4).min(jobs.max(1))
}

// ─── 界面状态 ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Status {
    Pending,
    Running,
    Done(Summary),
    Failed(String),
}

struct Item {
    path: PathBuf,
    name: String,
    status: Status,
}

#[derive(Clone, Copy, Debug)]
enum Msg {
    Pick,
    Start,
    SaveGallery,
    ToggleTheme,
    SetPrimaryQuality(u8),
    SetGainQuality(u8),
    SetScale(u32),
    SetAutoTone(bool),
    SetMaxBoost(f32),
    SetKnee(f32),
    SetProgressive(bool),
    SetDisplayP3(bool),
    SetKeepExif(bool),
    Remove(usize),
    /// 选中哪一张作为预览与绘制的对象。
    Select(usize),

    // ── 蒙版编辑器 ──
    SetTool(u8),
    SetOverlay(u32),
    SetSimulate(bool),
    SetExposure(f32),
    /// 拖拽结束，把草稿层提交进蒙版栈。
    CommitDraft,
    CancelDraft,
    SelectLayer(usize),
    RemoveLayer(usize),
    MoveLayer(usize),
    ToggleLayer(usize),
    CycleCombine(usize),
    ToggleInvert(usize),
    ToggleFollowBase(usize),
    SetOpacity(usize, f32),
    SetLayerBoost(usize, f32),
    SetLayerKnee(usize, f32),
    SetFeather(usize, f32),
    SetBrushRadius(f32),
    SetBrushHardness(f32),
    SetBrushFlow(f32),
    SetBrushErase(bool),
    UndoStroke,
    ClearBrush,
    /// 亮度蒙版的三个参数：下限、上限、过渡。
    SetLuma(usize, f32, f32, f32),
    /// 颜色蒙版的四个参数：色相、容差、最低饱和、过渡。
    SetHue(usize, f32, f32, f32, f32),
    /// 开始给某一层改名；文本本身存在编辑器里（`Msg` 要 `Copy`）。
    BeginRename(usize),
    /// 改名输入框失去焦点或按下回车。
    CommitRename,
}

struct Root {
    items: Vec<Item>,
    settings: Settings,
    dark: bool,
    notice: String,
    running: bool,
    finished: usize,
    awaiting_pick: bool,
    gallery_saved: usize,

    // ── 预览与蒙版 ──
    selected: usize,
    layer_selected: usize,
    /// 已解码的低分辨率副本，蒙版几何与叠加层都基于它。
    frame: RefCell<Option<FrameEntry>>,
    editor: RefCell<MaskUi>,
}

/// 缓存的一份预览帧，`key` 是文件路径，用来判断何时需要重新解码。
struct FrameEntry {
    key: String,
    frame: PreviewFrame,
}

impl Root {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            settings: Settings::default(),
            dark: true,
            notice: String::new(),
            running: false,
            finished: 0,
            awaiting_pick: false,
            gallery_saved: 0,
            selected: 0,
            layer_selected: 0,
            frame: RefCell::new(None),
            editor: RefCell::new(MaskUi::default()),
        }
    }

    /// 把（可能失效的）预览帧换成当前选中文件的缩略图。
    fn ensure_frame(&self) {
        let Some(item) = self.items.get(self.selected) else {
            let mut slot = self.frame.borrow_mut();
            if slot.is_some() {
                *slot = None;
                self.editor.borrow_mut().reset();
            }
            return;
        };
        let wanted = item.path.display().to_string();
        if self
            .frame
            .borrow()
            .as_ref()
            .is_some_and(|entry| entry.key == wanted)
        {
            return;
        }
        // 换图时草稿层属于上一张，必须丢掉，否则会画到新图上。
        self.editor.borrow_mut().reset();
        let entry = match PreviewFrame::load(&item.path, 1080) {
            Ok(frame) => Some(FrameEntry {
                key: wanted,
                frame,
            }),
            Err(err) => {
                log::warn!("预览帧解码失败：{err:#}");
                None
            }
        };
        *self.frame.borrow_mut() = entry;
    }

    /// 编辑器要用的提亮曲线：自动模式按这张缩略图的分位算，
    /// 结果与全分辨率一致到滑块精度以内。
    fn current_tone(&self, frame: &PreviewFrame) -> ToneParams {
        let mut tone = if self.settings.auto_tone {
            frame.tone()
        } else {
            ToneParams {
                knee: self.settings.knee,
                max_boost: self.settings.max_boost,
                top_level: 1.0,
                top_stops: self.settings.max_boost.log2(),
            }
        };
        let wanted = self.settings.masks.capacity(tone);
        if wanted > tone.max_boost {
            tone.max_boost = wanted;
            tone.top_stops = tone.max_boost.log2();
        }
        tone
    }


    /// 蒙版栈在 `Arc` 里（转换时每个 worker 都 clone 一份 Settings），
    /// 编辑前要先拿到独占引用。
    fn masks_mut(&mut self) -> &mut MaskStack {
        std::sync::Arc::make_mut(&mut self.settings.masks)
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        for path in paths {
            if self.items.iter().any(|item| item.path == path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            self.items.push(Item {
                path,
                name,
                status: Status::Pending,
            });
            added += 1;
        }
        if added > 0 {
            self.notice = format!("已添加 {added} 张图片");
        } else if !self.awaiting_pick {
            self.notice = "没有新的图片被选中".to_owned();
        }
    }

    fn outputs(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .filter_map(|item| match &item.status {
                Status::Done(summary) => Some(summary.output.clone()),
                _ => None,
            })
            .collect()
    }

    /// 预览 + 蒙版绘制。这是手机上唯一能「看到」照片的地方。
    ///
    /// 画布用的是 1080 px 的缩略副本，但蒙版的几何全部按 0..1 归一化保存，
    /// 所以在这里画的位置就是导出时全分辨率图上生效的位置。
    fn preview_card(&self, ui: &mut egui::Ui, m: &M3, dispatch: &Dispatcher<Msg>) {
        let frame_ref = self.frame.borrow();
        let Some(entry) = frame_ref.as_ref() else {
            section_title(ui, m, "蒙版", None);
            ui.label(
                egui::RichText::new("正在解码预览…")
                    .size(12.0)
                    .color(m.on_surface_variant),
            );
            return;
        };
        let frame = &entry.frame;
        let tone = self.current_tone(frame);
        let mut editor = self.editor.borrow_mut();

        editor.ensure_texture(
            ui.ctx(),
            frame,
            &self.settings.masks,
            tone,
            self.layer_selected,
        );

        // ── 工具条 ──────────────────────────────────────────────────
        section_title(
            ui,
            m,
            "蒙版",
            Some(&format!("{} 个", self.settings.masks.len())),
        );

        let mut scroll = egui::ScrollArea::horizontal().id_salt("mask_tools");
        scroll = scroll.auto_shrink([false, true]);
        scroll.show(ui, |ui| {
            ui.horizontal(|ui| {
                for tool in Tool::ALL {
                    let active = editor.tool == tool;
                    let text_color = if active { m.on_primary } else { m.on_surface_variant };
                    let galley = ui.painter().layout_no_wrap(
                        tool.label().to_owned(),
                        egui::FontId::proportional(12.5),
                        text_color,
                    );
                    let size = egui::vec2(galley.size().x + 22.0, 34.0);
                    let (rect, response) =
                        ui.allocate_exact_size(size, egui::Sense::click());
                    let bg = if active {
                        m.primary
                    } else if response.hovered() {
                        m.surface_variant
                    } else {
                        m.surface_container_highest
                    };
                    let origin = rect.center() - galley.size() / 2.0;
                    let painter = ui.painter();
                    painter.rect_filled(rect, 17.0, bg);
                    painter.galley(origin, galley, text_color);
                    if response.clicked() {
                        dispatch.dispatch(Msg::SetTool(tool.index()));
                    }
                }
            });
        });

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(editor.tool.hint())
                .size(11.5)
                .color(m.on_surface_variant),
        );
        ui.add_space(6.0);

        // ── 画布 ────────────────────────────────────────────────────
        let available = ui.available_width();
        let aspect = (frame.height as f32 / frame.width.max(1) as f32).clamp(0.2, 4.0);
        let height = (available * aspect).clamp(140.0, 420.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(available, height), egui::Sense::click_and_drag());

        let tex_size = egui::vec2(frame.width as f32, frame.height as f32);
        let scale = (rect.width() / tex_size.x).min(rect.height() / tex_size.y);
        let fitted = egui::Rect::from_center_size(
            rect.center(),
            egui::vec2(tex_size.x * scale, tex_size.y * scale),
        );

        let painter = ui.painter();
        painter.rect_filled(rect, 12.0, m.surface_container_highest);
        match editor.texture_id() {
            Some(id) => {
                painter.image(
                    id,
                    fitted,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                painter.text(
                    fitted.center(),
                    egui::Align2::CENTER_CENTER,
                    "这张图无法预览",
                    egui::FontId::proportional(13.0),
                    m.on_surface_variant,
                );
            }
        }

        let to_norm = |pos: egui::Pos2| -> [f32; 2] {
            [
                ((pos.x - fitted.left()) / fitted.width().max(1.0)).clamp(0.0, 1.0),
                ((pos.y - fitted.top()) / fitted.height().max(1.0)).clamp(0.0, 1.0),
            ]
        };
        let sample = |point: [f32; 2]| -> (f32, [u8; 3]) {
            let x = ((point[0] * frame.width as f32) as u32).min(frame.width.saturating_sub(1));
            let y = ((point[1] * frame.height as f32) as u32).min(frame.height.saturating_sub(1));
            let offset = (y as usize * frame.width as usize + x as usize) * 3;
            let rgb = [
                frame.rgb[offset],
                frame.rgb[offset + 1],
                frame.rgb[offset + 2],
            ];
            (frame.level_at(x as usize, y as usize), rgb)
        };

        // 双指捏合：画笔时改笔刷粗细，其余工具改当前蒙版的羽化。
        let (zoom, touching) = ui.input(|i| (i.zoom_delta(), i.any_touches()));
        if touching && (zoom - 1.0).abs() > 0.01 {
            if editor.tool == Tool::Brush {
                editor.brush_radius = (editor.brush_radius * zoom).clamp(0.005, 0.5);
            } else if let Some(layer) = self.settings.masks.layers().get(self.layer_selected) {
                let next = (layer.shape.feather() * zoom).clamp(0.0, 1.0);
                dispatch.dispatch(Msg::SetFeather(self.layer_selected, next));
            }
        }

        let mut commit = false;

        // 手势判定必须读原始指针状态，不能看 `response.dragged()`。
        //
        // 画布在 ScrollArea 里，手机上手指一落，ScrollArea 的触摸拖动
        // （`DragScroll::OnTouch`）就把这条手势认领走，`response` 再也不会
        // 进入 dragged 状态 —— 于是线性/径向蒙版的终点永远停在起点，权重
        // 恒为 0（列表里能看到「1 个」，画面上却什么都没有）。
        // 这里和 design.rs 的 slider 用同一套办法：按下点落在画布里才认这条手势。
        let (origin, latest, down, delta) = ui.input(|i| {
            (
                i.pointer.press_origin(),
                i.pointer.latest_pos(),
                i.pointer.primary_down(),
                i.pointer.delta(),
            )
        });
        let delta_norm = egui::vec2(
            delta.x / fitted.width().max(1.0),
            delta.y / fitted.height().max(1.0),
        );

        if down {
            if let (Some(from), Some(pos)) = (origin, latest) {
                if fitted.contains(from) {
                    if !editor.is_dragging() {
                        let point = to_norm(from);
                        let (level, rgb) = sample(point);
                        editor.begin(point, level, rgb, tone, self.settings.masks.len() + 1);
                        // 起手就落在画布上，立刻把终点推到位（轻点也能建蒙版）。
                        editor.drag_to(to_norm(pos), point);
                    } else {
                        let point = to_norm(pos);
                        let from = [
                            (point[0] - delta_norm.x).clamp(0.0, 1.0),
                            (point[1] - delta_norm.y).clamp(0.0, 1.0),
                        ];
                        editor.drag_to(point, from);
                    }
                }
            }
        } else if editor.is_dragging() {
            // 手指抬起：指针已经没有 latest_pos，用上一帧位置收尾。
            let point = latest.map(to_norm).unwrap_or([0.5, 0.5]);
            // 最后一段位移经常和抬手落在同一批事件里，这一帧不再走上面的
            // drag_to，线性/径向的终点就停在上一次采样处 —— 快速横划时看起来
            // 像「画了个点」。收尾前先把终点推到位。
            editor.drag_to(point, point);
            editor.finish(point);
            commit = true;
        }
        if down {
            // 手指按住期间主动要求重绘，否则空闲时框架的 repaint_delay 是
            // 无限大，移动事件攒到最后才处理，采样点会稀疏。
            ui.ctx().request_repaint();
        }

        // 画笔工具下画一圈光标，让手指知道笔有多大。
        if editor.tool == Tool::Brush {
            if let Some(pos) = response.hover_pos().filter(|p| fitted.contains(*p)) {
                let radius = editor.brush_radius * fitted.width();
                painter.circle_stroke(
                    pos,
                    radius,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 176, 64)),
                );
            }
        }

        if commit {
            dispatch.dispatch(Msg::CommitDraft);
        }

        ui.label(
            egui::RichText::new(format!(
                "{}×{} · 拖动画蒙版，双指捏合调{}",
                frame.width,
                frame.height,
                if editor.tool == Tool::Brush {
                    "笔刷大小"
                } else {
                    "羽化"
                }
            ))
            .size(11.0)
            .color(m.on_surface_variant),
        );
        ui.add_space(8.0);

        // ── 显示模式 + HDR 模拟 ─────────────────────────────────────
        // 三个模式并排：必须给每个按钮定宽，否则第一个吃掉整行。
        let modes = [Overlay::All, Overlay::Selected, Overlay::None];
        let items: Vec<(String, ButtonKind, bool)> = modes
            .iter()
            .map(|mode| {
                let kind = if editor.overlay == *mode {
                    ButtonKind::Filled
                } else {
                    ButtonKind::Text
                };
                (mode.label().to_owned(), kind, true)
            })
            .collect();
        if let Some(hit) = design::button_row(ui, m, &items) {
            dispatch.dispatch(Msg::SetOverlay(modes[hit].index()));
        }
        ui.add_space(4.0);

        let mut simulate = editor.simulate;
        if switch(
            ui,
            m,
            "simulate",
            "HDR 预览",
            Some("按当前蒙版和提亮参数模拟 HDR 观感"),
            &mut simulate,
        ) {
            dispatch.dispatch(Msg::SetSimulate(simulate));
        }
        if editor.simulate {
            let mut exposure = editor.exposure;
            if slider(ui, m, "预览亮度", &mut exposure, 0.2..=4.0, 2, "×") {
                dispatch.dispatch(Msg::SetExposure(exposure));
            }
        }
    }

    /// 蒙版列表 + 当前层的参数。
    fn mask_card(&self, ui: &mut egui::Ui, m: &M3, dispatch: &Dispatcher<Msg>) {
        let editor = self.editor.borrow();
        section_title(ui, m, "蒙版图层", Some(&format!("{} 个", self.settings.masks.len())));

        if self.settings.masks.is_empty() {
            ui.label(
                egui::RichText::new("选一个工具，在上面的预览图上拖一下就能建一个蒙版。")
                    .size(12.0)
                    .color(m.on_surface_variant),
            );
            return;
        }

        for (index, layer) in self.settings.masks.layers().iter().enumerate() {
            let selected = index == self.layer_selected;
            let row_bg = if selected {
                mix(m.primary, m.surface, 0.22)
            } else {
                egui::Color32::TRANSPARENT
            };
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 46.0),
                egui::Sense::click(),
            );
            if row_bg != egui::Color32::TRANSPARENT {
                ui.painter().rect_filled(rect, 12.0, row_bg);
            } else if response.hovered() {
                ui.painter().rect_filled(rect, 12.0, m.surface_variant);
            }

            // 启用开关
            let switch_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 26.0, rect.center().y),
                egui::vec2(40.0, 24.0),
            );
            let switch_response =
                ui.interact(switch_rect, ui.id().with(("layer_on", index)), egui::Sense::click());
            // 选中行是浅紫底：开启态改用深色轨道 + 浅色圆点，否则浅紫压浅紫看不出来。
            let (track, thumb) = match (layer.enabled, selected) {
                (true, false) => (m.primary, m.surface),
                (true, true) => (m.on_primary, m.primary),
                (false, true) => (m.on_primary.gamma_multiply(0.45), m.on_primary),
                (false, false) => (m.outline_variant, m.surface),
            };
            ui.painter().rect_filled(switch_rect, 12.0, track);
            let thumb_x = if layer.enabled {
                switch_rect.right() - 12.0
            } else {
                switch_rect.left() + 12.0
            };
            ui.painter()
                .circle_filled(egui::pos2(thumb_x, switch_rect.center().y), 9.0, thumb);
            if switch_response.clicked() {
                dispatch.dispatch(Msg::ToggleLayer(index));
            }

            // 选中行是浅紫底，文字必须换成 on_primary，否则浅底浅字看不清。
            let (title_color, sub_color) = if selected {
                (m.on_primary, m.on_primary.gamma_multiply(0.82))
            } else {
                (m.on_surface, m.on_surface_variant)
            };
            let name_x = rect.left() + 54.0;
            ui.painter().text(
                egui::pos2(name_x, rect.center().y - 8.0),
                egui::Align2::LEFT_CENTER,
                format!("{} {}", combine_label(layer.combine), layer.name),
                egui::FontId::proportional(13.5),
                title_color,
            );
            ui.painter().text(
                egui::pos2(name_x, rect.center().y + 10.0),
                egui::Align2::LEFT_CENTER,
                format!(
                    "{} · {:.0}%{}",
                    layer.shape.label(),
                    layer.opacity * 100.0,
                    if layer.follow_base { "" } else { " · 独立参数" }
                ),
                egui::FontId::proportional(11.0),
                sub_color,
            );

            // 右侧：下移 + 删除
            let close = egui::Rect::from_center_size(
                egui::pos2(rect.right() - 20.0, rect.center().y),
                egui::vec2(34.0, 34.0),
            );
            let close_response =
                ui.interact(close, ui.id().with(("layer_del", index)), egui::Sense::click());
            if close_response.hovered() {
                ui.painter().circle_filled(close.center(), 15.0, m.surface_variant);
            }
            let painter = ui.painter();
            let c = close.center();
            let s = 6.0;
            // 选中行是浅紫底，图标也要跟着变深，否则悬停圈和叉号都看不清。
            let color = title_color;
            painter.line_segment(
                [egui::pos2(c.x - s, c.y - s), egui::pos2(c.x + s, c.y + s)],
                egui::Stroke::new(1.8, color),
            );
            painter.line_segment(
                [egui::pos2(c.x + s, c.y - s), egui::pos2(c.x - s, c.y + s)],
                egui::Stroke::new(1.8, color),
            );

            // 蒙版的先后决定叠加顺序（后画的覆盖前面的），所以要能调。
            let mut moved = false;
            if index + 1 < self.settings.masks.len() {
                let down = egui::Rect::from_center_size(
                    egui::pos2(rect.right() - 56.0, rect.center().y),
                    egui::vec2(30.0, 34.0),
                );
                let down_response =
                    ui.interact(down, ui.id().with(("layer_down", index)), egui::Sense::click());
                if down_response.hovered() {
                    ui.painter().circle_filled(down.center(), 14.0, m.surface_variant);
                }
                let arrow = ui.painter();
                let origin = down.center();
                arrow.line_segment(
                    [
                        egui::pos2(origin.x, origin.y - 6.0),
                        egui::pos2(origin.x, origin.y + 6.0),
                    ],
                    egui::Stroke::new(1.8, color),
                );
                arrow.line_segment(
                    [
                        egui::pos2(origin.x - 4.5, origin.y + 2.0),
                        egui::pos2(origin.x, origin.y + 6.0),
                    ],
                    egui::Stroke::new(1.8, color),
                );
                arrow.line_segment(
                    [
                        egui::pos2(origin.x + 4.5, origin.y + 2.0),
                        egui::pos2(origin.x, origin.y + 6.0),
                    ],
                    egui::Stroke::new(1.8, color),
                );
                moved = down_response.clicked();
            }

            if moved {
                dispatch.dispatch(Msg::MoveLayer(index));
            } else if close_response.clicked() {
                dispatch.dispatch(Msg::RemoveLayer(index));
            } else if response.double_clicked() {
                dispatch.dispatch(Msg::BeginRename(index));
            } else if response.clicked() {
                dispatch.dispatch(Msg::SelectLayer(index));
            }
        }

        // 改名输入框：`Msg` 要 `Copy`，所以文本草图放在编辑器里。
        let renaming = self.editor.borrow().rename.clone();
        if let Some((index, text)) = renaming {
            let mut draft = text;
            let response = ui.add(
                egui::TextEdit::singleline(&mut draft)
                    .hint_text("蒙版名称")
                    .desired_width(f32::INFINITY),
            );
            if let Some(slot) = self.editor.borrow_mut().rename.as_mut() {
                slot.1 = draft;
            }
            if response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                dispatch.dispatch(Msg::CommitRename);
            }
            ui.add_space(4.0);
        }

        ui.add_space(8.0);
        let Some(layer) = self.settings.masks.layers().get(self.layer_selected).cloned() else {
            return;
        };

        // ── 当前层参数 ──────────────────────────────────────────────
        ui.label(
            egui::RichText::new(format!("「{}」· {}", layer.name, layer.shape.label()))
                .size(13.0)
                .color(m.on_surface),
        );
        ui.add_space(6.0);

        // 三种叠加方式并排：同样要定宽。
        let combines = [
            MaskCombine::Add,
            MaskCombine::Subtract,
            MaskCombine::Intersect,
        ];
        let items: Vec<(String, ButtonKind, bool)> = combines
            .iter()
            .map(|combine| {
                let kind = if layer.combine == *combine {
                    ButtonKind::Filled
                } else {
                    ButtonKind::Text
                };
                (combine_label(*combine).to_owned(), kind, true)
            })
            .collect();
        if design::button_row(ui, m, &items).is_some() {
            dispatch.dispatch(Msg::CycleCombine(self.layer_selected));
        }

        ui.add_space(4.0);
        let mut invert = layer.invert;
        if switch(ui, m, "invert", "反转", Some("把选中的区域换成它的补集"), &mut invert) {
            dispatch.dispatch(Msg::ToggleInvert(self.layer_selected));
        }

        let mut opacity = layer.opacity * 100.0;
        if slider(ui, m, "不透明度", &mut opacity, 0.0..=100.0, 0, "%") {
            dispatch.dispatch(Msg::SetOpacity(self.layer_selected, (opacity / 100.0).clamp(0.0, 1.0)));
        }

        let mut feather = layer.shape.feather();
        if slider(ui, m, "羽化", &mut feather, 0.0..=1.0, 2, "") {
            dispatch.dispatch(Msg::SetFeather(self.layer_selected, feather));
        }

        let mut follow = layer.follow_base;
        if switch(
            ui,
            m,
            "follow",
            "跟随全局提亮",
            Some("关闭后这个蒙版用自己的上限与起点"),
            &mut follow,
        ) {
            dispatch.dispatch(Msg::ToggleFollowBase(self.layer_selected));
        }

        if layer.follow_base {
            ui.label(
                egui::RichText::new(format!(
                    "起点 {:.2} · 上限 {:.2}×（与全局相同）",
                    layer.knee, layer.max_boost
                ))
                .size(11.5)
                .color(m.on_surface_variant),
            );
        } else {
            let mut boost = layer.max_boost;
            if slider(ui, m, "提亮上限", &mut boost, 1.0..=8.0, 2, "×") {
                dispatch.dispatch(Msg::SetLayerBoost(self.layer_selected, boost));
            }
            let mut knee = layer.knee;
            if slider(ui, m, "提亮起点", &mut knee, 0.0..=0.95, 2, "") {
                dispatch.dispatch(Msg::SetLayerKnee(self.layer_selected, knee));
            }
        }

        // ── 形状专属 ────────────────────────────────────────────────
        ui.add_space(6.0);
        match &layer.shape {
            MaskShape::Linear { .. } | MaskShape::Radial { .. } => {
                ui.label(
                    egui::RichText::new("在预览图上再拖一次可以重新摆放，双指捏合调羽化。")
                        .size(11.5)
                        .color(m.on_surface_variant),
                );
            }
            MaskShape::Luminance {
                min,
                max,
                softness,
            } => {
                let (mut low, mut high, mut soft) = (*min, *max, *softness);
                let mut changed = false;
                changed |= slider(ui, m, "亮度下限", &mut low, 0.0..=1.0, 2, "");
                changed |= slider(ui, m, "亮度上限", &mut high, 0.0..=1.0, 2, "");
                changed |= slider(ui, m, "过渡", &mut soft, 0.0..=1.0, 2, "");
                if changed {
                    dispatch.dispatch(Msg::SetLuma(self.layer_selected, low, high, soft));
                }
            }
            MaskShape::Color {
                hue,
                hue_tolerance,
                saturation_min,
                softness,
            } => {
                let (mut h, mut tol, mut sat, mut soft) =
                    (*hue, *hue_tolerance, *saturation_min, *softness);
                let mut changed = false;
                changed |= slider(ui, m, "色相", &mut h, 0.0..=1.0, 2, "");
                changed |= slider(ui, m, "容差", &mut tol, 0.005..=0.5, 3, "");
                changed |= slider(ui, m, "最低饱和", &mut sat, 0.0..=0.95, 2, "");
                changed |= slider(ui, m, "过渡", &mut soft, 0.0..=1.0, 2, "");
                if changed {
                    dispatch.dispatch(Msg::SetHue(self.layer_selected, h, tol, sat, soft));
                }

                // 色相条：让人看出当前选的是哪一段颜色。
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
                let steps = 48;
                for step in 0..steps {
                    let t = step as f32 / steps as f32;
                    let (r, g, b) = mask_ui::hue_rgb(t);
                    let x0 = rect.left() + rect.width() * step as f32 / steps as f32;
                    let x1 = rect.left() + rect.width() * (step + 1) as f32 / steps as f32;
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
                        0.0,
                        egui::Color32::from_rgb(r, g, b),
                    );
                }
                let marker_x = rect.left() + rect.width() * *hue;
                ui.painter().line_segment(
                    [
                        egui::pos2(marker_x, rect.top() - 2.0),
                        egui::pos2(marker_x, rect.bottom() + 2.0),
                    ],
                    egui::Stroke::new(2.0, egui::Color32::WHITE),
                );
            }
            MaskShape::Brush(mask) => {
                ui.label(
                    egui::RichText::new(format!("{} 笔", mask.strokes.len()))
                        .size(11.5)
                        .color(m.on_surface_variant),
                );
                let mut radius = editor.brush_radius;
                if slider(ui, m, "笔刷大小", &mut radius, 0.005..=0.5, 3, "") {
                    dispatch.dispatch(Msg::SetBrushRadius(radius));
                }
                let mut hardness = editor.brush_hardness;
                if slider(ui, m, "硬度", &mut hardness, 0.0..=1.0, 2, "") {
                    dispatch.dispatch(Msg::SetBrushHardness(hardness));
                }
                let mut flow = editor.brush_flow;
                if slider(ui, m, "不透明度", &mut flow, 0.05..=1.0, 2, "") {
                    dispatch.dispatch(Msg::SetBrushFlow(flow));
                }
                let mut erase = editor.brush_erase;
                if switch(ui, m, "erase", "橡皮擦", Some("擦掉已经画上的部分"), &mut erase) {
                    dispatch.dispatch(Msg::SetBrushErase(erase));
                }
                let has_strokes = !mask.is_empty();
                let items: Vec<(String, ButtonKind, bool)> = vec![
                    ("撤销一笔".to_owned(), ButtonKind::Text, has_strokes),
                    ("清空".to_owned(), ButtonKind::Text, has_strokes),
                ];
                match design::button_row(ui, m, &items) {
                    Some(0) => dispatch.dispatch(Msg::UndoStroke),
                    Some(_) => dispatch.dispatch(Msg::ClearBrush),
                    None => {}
                }
            }
        }
    }
}

impl LifecycleObserver for Root {}

impl Component for Root {
    type State = ();
    type Message = Msg;

    fn render(&self, ui: &mut UiWrapper, dispatch: &Dispatcher<Self::Message>) {
        let theme = Theme::current_from_ui(ui);
        let m = M3::new(&theme.colors, self.dark);

        // 整窗铺底色，避免面板之外的区域透出系统背景。
        ui.painter()
            .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, m.surface);

        // ── 顶部应用栏 ──────────────────────────────────────────────
        egui::Panel::top("m3_top_bar")
            .frame(
                egui::Frame::new()
                    .fill(m.surface)
                    .inner_margin(egui::Margin::symmetric(20, 0)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                if top_bar(ui, &m, "JPG → Ultra HDR", "把普通 JPEG 变成 HDR 照片") {
                    dispatch.dispatch(Msg::ToggleTheme);
                }
            });

        // ── 底部操作栏 ──────────────────────────────────────────────
        let done_count = self
            .items
            .iter()
            .filter(|item| matches!(item.status, Status::Done(_)))
            .count();
        let can_start = !self.items.is_empty() && !self.running;

        egui::Panel::bottom("m3_bottom_bar")
            .frame(
                egui::Frame::new()
                    .fill(m.surface_container_low)
                    .stroke(egui::Stroke::new(1.0, m.outline_variant))
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                if self.running {
                    let fraction = if self.items.is_empty() {
                        0.0
                    } else {
                        self.finished as f32 / self.items.len() as f32
                    };
                    linear_progress(ui, &m, fraction);
                    ui.add_space(12.0);
                }

                let total = ui.available_width();
                let gap = 10.0;
                let half = ((total - gap) / 2.0).max(0.0);

                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 52.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_min_width(half);
                            if button(ui, &m, "添加图片", ButtonKind::Outlined, true) {
                                dispatch.dispatch(Msg::Pick);
                            }
                        },
                    );
                    ui.add_space(gap);
                    ui.allocate_ui_with_layout(
                        egui::vec2(half, 52.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_min_width(half);
                            if button(
                                ui,
                                &m,
                                "保存到相册",
                                ButtonKind::Outlined,
                                done_count > 0,
                            ) {
                                dispatch.dispatch(Msg::SaveGallery);
                            }
                        },
                    );
                });

                ui.add_space(10.0);
                let label = if self.running {
                    format!("转换中… {}/{}", self.finished, self.items.len())
                } else {
                    "开始转换".to_owned()
                };
                if button(ui, &m, label, ButtonKind::Filled, can_start) {
                    dispatch.dispatch(Msg::Start);
                }
            });

        // ── 主体 ────────────────────────────────────────────────────
        // 右边留 10 px：滚动条浮在内容之上，不留白会把右对齐的数值裁掉。
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 0,
                right: 10,
                top: 0,
                bottom: 0,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(14.0);

                        if self.items.is_empty() {
                            card(ui, &m, false, |ui| empty_state(ui, &m));
                        } else {
                            card(ui, &m, false, |ui| {
                                section_title(
                                    ui,
                                    &m,
                                    "待转换图片",
                                    Some(&format!("{} 张", self.items.len())),
                                );
                                for (index, item) in self.items.iter().enumerate() {
                                    file_row(ui, &m, index, item, index == self.selected, dispatch);
                                }
                            });

                            ui.add_space(12.0);
                            self.ensure_frame();
                            card(ui, &m, false, |ui| self.preview_card(ui, &m, dispatch));
                            ui.add_space(12.0);
                            card(ui, &m, false, |ui| self.mask_card(ui, &m, dispatch));
                        }

                        ui.add_space(12.0);
                        card(ui, &m, false, |ui| {
                            settings_card(ui, &m, &self.settings, dispatch);
                        });

                        if !self.notice.is_empty() {
                            ui.add_space(12.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(&self.notice)
                                        .size(12.5)
                                        .color(m.on_surface_variant),
                                );
                            });
                        }

                        ui.add_space(20.0);
                    });
            });
    }


    fn handle(&mut self, msg: Self::Message) {
        match msg {
            Msg::Pick => {
                #[cfg(target_os = "android")]
                jni_bridge::pick_images();
                #[cfg(not(target_os = "android"))]
                {
                    self.notice = "此构建不支持文件选择".to_owned();
                }
                self.awaiting_pick = true;
                self.notice = "请选择一张或多张 JPEG".to_owned();
            }
            Msg::Start => {}
            Msg::SaveGallery => {}
            Msg::ToggleTheme => self.dark = !self.dark,
            Msg::SetPrimaryQuality(v) => self.settings.primary_quality = v,
            Msg::SetGainQuality(v) => self.settings.gainmap_quality = v,
            Msg::SetScale(v) => self.settings.scale = v,
            Msg::SetAutoTone(v) => self.settings.auto_tone = v,
            Msg::SetMaxBoost(v) => self.settings.max_boost = v,
            Msg::SetKnee(v) => self.settings.knee = v,
            Msg::SetProgressive(v) => self.settings.progressive = v,
            Msg::SetDisplayP3(v) => self.settings.to_display_p3 = v,
            Msg::SetKeepExif(v) => self.settings.keep_exif = v,
            Msg::Remove(index) => {
                if index < self.items.len() {
                    self.items.remove(index);
                    if self.selected >= self.items.len() {
                        self.selected = self.items.len().saturating_sub(1);
                    }
                    self.notice = format!("还剩 {} 张图片", self.items.len());
                }
            }
            Msg::Select(index) => {
                if index < self.items.len() {
                    self.selected = index;
                    self.notice = format!("正在编辑第 {} 张的蒙版", index + 1);
                }
            }
            Msg::SetTool(value) => {
                self.editor.borrow_mut().cancel_drag();
                self.editor.borrow_mut().tool = Tool::from_index(value);
            }
            Msg::SetOverlay(value) => self.editor.borrow_mut().overlay = Overlay::from_index(value),
            Msg::SetSimulate(value) => self.editor.borrow_mut().simulate = value,
            Msg::SetExposure(value) => self.editor.borrow_mut().exposure = value.clamp(0.2, 4.0),
            Msg::CommitDraft => {
                let pending = self.editor.borrow_mut().take_pending();
                if let Some(layer) = pending {
                    self.masks_mut().push(layer);
                    let count = self.settings.masks.len();
                    self.layer_selected = count.saturating_sub(1);
                    self.notice = format!("已建第 {count} 个蒙版");
                }
            }
            Msg::CancelDraft => self.editor.borrow_mut().cancel_drag(),
            Msg::SelectLayer(index) => {
                if index < self.settings.masks.len() {
                    self.layer_selected = index;
                }
            }
            Msg::RemoveLayer(index) => {
                let removed = self.masks_mut().remove(index);
                if removed {
                    let left = self.settings.masks.len();
                    self.layer_selected = self.layer_selected.min(left.saturating_sub(1));
                    self.notice = format!("还剩 {left} 个蒙版");
                }
            }
            Msg::MoveLayer(index) => {
                let layers = self.masks_mut().layers_mut();
                if index + 1 < layers.len() {
                    layers.swap(index, index + 1);
                    self.layer_selected = index + 1;
                }
            }
            Msg::ToggleLayer(index) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.enabled = !layer.enabled;
                }
            }
            Msg::CycleCombine(index) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.combine = match layer.combine {
                        MaskCombine::Add => MaskCombine::Subtract,
                        MaskCombine::Subtract => MaskCombine::Intersect,
                        MaskCombine::Intersect => MaskCombine::Add,
                    };
                }
            }
            Msg::ToggleInvert(index) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.invert = !layer.invert;
                }
            }
            Msg::ToggleFollowBase(index) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.follow_base = !layer.follow_base;
                }
            }
            Msg::SetOpacity(index, value) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.opacity = value.clamp(0.0, 1.0);
                }
            }
            Msg::SetLayerBoost(index, value) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.max_boost = value.clamp(1.0, 16.0);
                }
            }
            Msg::SetLayerKnee(index, value) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.knee = value.clamp(0.0, 0.99);
                }
            }
            Msg::SetFeather(index, value) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    layer.shape.set_feather(value);
                }
            }
            Msg::SetBrushRadius(v) => self.editor.borrow_mut().brush_radius = v.clamp(0.005, 0.5),
            Msg::SetBrushHardness(v) => {
                self.editor.borrow_mut().brush_hardness = v.clamp(0.0, 1.0)
            }
            Msg::SetBrushFlow(v) => self.editor.borrow_mut().brush_flow = v.clamp(0.05, 1.0),
            Msg::SetBrushErase(v) => self.editor.borrow_mut().brush_erase = v,
            Msg::UndoStroke => {
                let selected = self.layer_selected;
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(selected) {
                    if let MaskShape::Brush(mask) = &mut layer.shape {
                        mask.undo();
                    }
                }
            }
            Msg::ClearBrush => {
                let selected = self.layer_selected;
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(selected) {
                    if let MaskShape::Brush(mask) = &mut layer.shape {
                        mask.clear();
                    }
                }
            }
            Msg::SetLuma(index, low, high, soft) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    if let MaskShape::Luminance { min, max, softness } = &mut layer.shape {
                        *min = low.clamp(0.0, 1.0);
                        *max = high.clamp(*min, 1.0);
                        *softness = soft.clamp(0.0, 1.0);
                    }
                }
            }
            Msg::SetHue(index, hue, tolerance, saturation, soft) => {
                if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                    if let MaskShape::Color {
                        hue: h,
                        hue_tolerance,
                        saturation_min,
                        softness,
                    } = &mut layer.shape
                    {
                        *h = hue.rem_euclid(1.0);
                        *hue_tolerance = tolerance.clamp(0.001, 0.5);
                        *saturation_min = saturation.clamp(0.0, 0.95);
                        *softness = soft.clamp(0.0, 1.0);
                    }
                }
            }
            Msg::BeginRename(index) => {
                let name = self
                    .settings
                    .masks
                    .layers()
                    .get(index)
                    .map(|layer| layer.name.clone())
                    .unwrap_or_default();
                self.editor.borrow_mut().rename = Some((index, name));
            }
            Msg::CommitRename => {
                let draft = self.editor.borrow_mut().rename.take();
                if let Some((index, name)) = draft {
                    let trimmed = name.trim().to_owned();
                    if !trimmed.is_empty() {
                        if let Some(layer) = self.masks_mut().layers_mut().get_mut(index) {
                            layer.name = trimmed;
                        }
                    }
                }
            }
        }
    }

    fn state(&self) -> &Self::State {
        &()
    }
}

// ─── 应用 ───────────────────────────────────────────────────────────

struct HdrApp {
    root: Root,
    config: RuntimeConfig,
    initialized: bool,
    /// 上一次灌入 egui 的深浅色，用来避免每帧重复应用样式。
    theme_applied: Option<bool>,
    cmd_tx: mpsc::Sender<Cmd>,
    evt_rx: mpsc::Receiver<Event>,
    wake_rx: Option<mpsc::Receiver<()>>,
}

impl HdrApp {
    fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.evt_rx.try_recv() {
            changed = true;
            match event {
                Event::Started(index) => {
                    if let Some(item) = self.root.items.get_mut(index) {
                        item.status = Status::Running;
                    }
                }
                Event::Done(index, result) => {
                    if let Some(item) = self.root.items.get_mut(index) {
                        item.status = match result {
                            Ok(summary) => Status::Done(summary),
                            Err(err) => Status::Failed(err),
                        };
                    }
                    self.root.finished += 1;
                }
                Event::AllDone => {
                    self.root.running = false;
                    self.root.notice = format!("全部完成：{} 张图片", self.root.finished);
                }
            }
        }
        changed
    }
}

impl Application for HdrApp {
    type RootComponent = Root;

    fn create() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = mpsc::channel::<Event>();
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || worker_loop(cmd_rx, evt_tx, wake_tx));

        Self {
            root: Root::new(),
            config: RuntimeConfig {
                log_tag: "jpg2ultrahdr".to_owned(),
                target_fps: 60,
                ..Default::default()
            },
            initialized: false,
            theme_applied: None,
            cmd_tx,
            evt_rx,
            wake_rx: Some(wake_rx),
        }
    }

    fn root(&mut self) -> &mut Root {
        &mut self.root
    }

    fn root_ref(&self) -> &Root {
        &self.root
    }

    fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    fn config_mut(&mut self) -> &mut RuntimeConfig {
        &mut self.config
    }

    fn create_runtime_context(&mut self, ctx: &egui::Context, wake: Waker) -> RuntimeContext {
        match self.wake_rx.take() {
            Some(rx) => RuntimeContext::new(Some(UiNotifier::new(ctx.clone(), Some(wake), rx))),
            None => RuntimeContext::new(None),
        }
    }

    fn frame(&mut self, ctx: &egui::Context, input: egui::RawInput) -> egui::FullOutput {
        // 主题只在初始化或用户切换时重新灌入样式；每帧都灌会白白复制两份 Style。
        if !self.initialized {
            install_cjk_font(ctx);
            MaterialTheme::dark().apply(ctx);
            self.theme_applied = Some(true);
            self.initialized = true;
        } else if self.theme_applied != Some(self.root.dark) {
            if self.root.dark {
                MaterialTheme::dark().apply(ctx);
            } else {
                MaterialTheme::light().apply(ctx);
            }
            self.theme_applied = Some(self.root.dark);
        }

        // 轮询系统选择器返回的路径
        if self.root.awaiting_pick {
            // 选择器是另一个 Activity，返回时不会产生 egui 的输入事件，
            // 所以要主动请求重绘，否则界面会一直等到用户触摸才刷新。
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            let paths = {
                #[cfg(target_os = "android")]
                {
                    jni_bridge::take_picked()
                }
                #[cfg(not(target_os = "android"))]
                {
                    Vec::new()
                }
            };
            if !paths.is_empty() {
                self.root.awaiting_pick = false;
                self.root.add_paths(paths);
            }
        }

        self.drain_events();

        // 框架的 UiNotifier::check() 只在 tick 里轮询，而空闲时 poll_events 会
        // 无限阻塞（repaint_delay = u64::MAX）。worker 线程的唤醒信号因此永远
        // 没机会被消费，界面会卡在“转换中”。这里在渲染期间主动设置心跳：
        // 转换中 150 ms，空闲 1 s。必须在 run_ui 内部设置，才能影响本帧输出。
        let (tx, rx) = Dispatcher::new();

        let output = ctx.run_ui(input, |ctx| {
            let heartbeat = if self.root.running { 150 } else { 1000 };
            ctx.request_repaint_after(std::time::Duration::from_millis(heartbeat));
            egui::CentralPanel::no_frame()
                .show(ctx, |ui| {
                    let mut wrapper = UiWrapper::new_unconstrained(ui);
                    self.root.render(&mut wrapper, &tx);
                });
        });

        for msg in rx.try_iter() {
            match msg {
                Msg::Start => {
                    self.start_conversion();
                    // 关键：本帧已经算完 repaint_delay（空闲时会阻塞下一次
                    // poll_events），必须立刻要求重绘，否则收不到进度事件。
                    ctx.request_repaint();
                }
                Msg::Pick => {
                    self.root.handle(Msg::Pick);
                    ctx.request_repaint();
                }
                Msg::SaveGallery => {
                    #[allow(unused_mut)]
                    let mut outputs = self.root.outputs();
                    if outputs.is_empty() {
                        self.root.notice = "还没有可保存的结果".to_owned();
                    } else {
                        #[cfg(target_os = "android")]
                        {
                            let count = jni_bridge::save_to_gallery(&outputs);
                            self.root.notice = if count == outputs.len() {
                                format!("已保存 {count} 张到相册 Pictures/JPG2UltraHDR")
                            } else {
                                format!("已保存 {count}/{} 张到相册", outputs.len())
                            };
                            self.root.gallery_saved += count;
                        }
                        #[cfg(not(target_os = "android"))]
                        {
                            self.root.notice = format!("有 {} 张结果待保存", outputs.len());
                        }
                    }
                    outputs.clear();
                }
                other => self.root.handle(other),
            }
        }

        output
    }

    fn on_back_pressed(&mut self) {
        // 交给系统处理，不在这里退出
    }
}

impl HdrApp {
    fn start_conversion(&mut self) {
        if self.root.running {
            return;
        }
        let jobs: Vec<(usize, PathBuf)> = self
            .root
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !matches!(item.status, Status::Done(_)))
            .map(|(index, item)| (index, item.path.clone()))
            .collect();

        if jobs.is_empty() {
            self.root.notice = "没有需要转换的图片".to_owned();
            return;
        }

        for (index, _) in &jobs {
            if let Some(item) = self.root.items.get_mut(*index) {
                item.status = Status::Pending;
            }
        }

        self.root.running = true;
        self.root.finished = 0;
        self.root.notice = format!("正在转换 {} 张图片…", jobs.len());
        let _ = self.cmd_tx.send(Cmd::Start(jobs, self.root.settings.clone()));
    }
}

// ─── 字体 ───────────────────────────────────────────────────────────

/// Android 系统自带中日韩字体，直接加载以免界面出现豆腐块。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 6] = [
        "/system/fonts/NotoSansCJK-Regular.ttc",
        "/system/fonts/NotoSansCJKsc-Regular.otf",
        "/system/fonts/NotoSansSC-Regular.otf",
        "/system/fonts/DroidSansFallbackFull.ttf",
        "/system/fonts/DroidSansFallback.ttf",
        "/system/fonts/NotoSansHans-Regular.otf",
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        log::info!("已加载中文字体 {path}");
        return;
    }
    log::warn!("未找到系统中文字体，界面可能显示方框");
}

// ─── 入口 ───────────────────────────────────────────────────────────

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    // 先把 JNI 句柄和输出目录存好，再把 app 交给框架（框架会接管所有权）。
    let vm_ptr = app.vm_as_ptr();
    let activity_ptr = app.activity_as_ptr();
    jni_bridge::init(vm_ptr, activity_ptr);

    let dir = app
        .internal_data_path()
        .map(|p| p.join("out"))
        .unwrap_or_else(|| std::env::temp_dir().join("jpg2ultrahdr-out"));
    let _ = std::fs::create_dir_all(&dir);
    let _ = OUT_DIR.set(dir);

    egui_android_framework::platform_android::run::<HdrApp>(app);
}


// ─── M3 界面片段 ─────────────────────────────────────────────────────

/// 空状态：圆角图标 + 两行引导文字。
fn empty_state(ui: &mut egui::Ui, m: &M3) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 170.0), egui::Sense::hover());
    let painter = ui.painter();

    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 52.0),
        egui::vec2(64.0, 64.0),
    );
    painter.rect_filled(icon, egui::CornerRadius::same(20), m.primary_container);
    let inner = icon.shrink(17.0);
    painter.rect_stroke(
        inner,
        egui::CornerRadius::same(6),
        egui::Stroke::new(1.8, m.on_primary_container),
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(
        egui::pos2(inner.left() + 8.0, inner.top() + 8.0),
        3.0,
        m.on_primary_container,
    );
    painter.line_segment(
        [
            egui::pos2(inner.left() + 2.0, inner.bottom() - 3.0),
            egui::pos2(inner.left() + 11.0, inner.bottom() - 12.0),
        ],
        egui::Stroke::new(1.8, m.on_primary_container),
    );
    painter.line_segment(
        [
            egui::pos2(inner.left() + 11.0, inner.bottom() - 12.0),
            egui::pos2(inner.right() - 2.0, inner.bottom() - 3.0),
        ],
        egui::Stroke::new(1.8, m.on_primary_container),
    );

    painter.text(
        egui::pos2(rect.center().x, rect.top() + 104.0),
        egui::Align2::CENTER_CENTER,
        "还没有添加图片",
        egui::FontId::proportional(15.0),
        m.on_surface,
    );
    painter.text(
        egui::pos2(rect.center().x, rect.top() + 128.0),
        egui::Align2::CENTER_CENTER,
        "点下方「添加图片」，从相册里多选 JPEG",
        egui::FontId::proportional(12.5),
        m.on_surface_variant,
    );
}

/// 单行文件条目：序号徽标 + 文件名 + 状态 + 移除按钮。
///
/// 点整行选中它作为蒙版编辑对象；右侧按钮只负责移除。
fn file_row(
    ui: &mut egui::Ui,
    m: &M3,
    index: usize,
    item: &Item,
    selected: bool,
    dispatch: &Dispatcher<Msg>,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 60.0), egui::Sense::hover());
    let painter = ui.painter();

    if selected {
        painter.rect_filled(
            rect,
            egui::CornerRadius::same(12),
            egui::Color32::from_rgba_unmultiplied(
                m.primary.r(),
                m.primary.g(),
                m.primary.b(),
                26,
            ),
        );
    }

    if index > 0 {
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.top()),
                egui::pos2(rect.right(), rect.top()),
            ],
            egui::Stroke::new(1.0, m.outline_variant),
        );
    }

    let badge = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::vec2(30.0, 30.0),
    );
    painter.rect_filled(
        badge,
        egui::CornerRadius::same(15),
        m.surface_container_highest,
    );
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        format!("{}", index + 1),
        egui::FontId::proportional(12.5),
        m.on_surface_variant,
    );

    let close = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 18.0, rect.center().y),
        egui::vec2(34.0, 34.0),
    );
    let close_response = ui.interact(close, ui.id().with(("remove", index)), egui::Sense::click());

    let text_left = rect.left() + 42.0;
    let text_right = close.left() - 8.0;
    let max_width = (text_right - text_left).max(24.0);

    let name_galley = ui.painter().layout(
        item.name.clone(),
        egui::FontId::proportional(14.0),
        m.on_surface,
        max_width,
    );
    ui.painter().galley(
        egui::pos2(text_left, rect.center().y - 15.0),
        name_galley,
        m.on_surface,
    );

    let (status_text, status_color) = match &item.status {
        Status::Pending => ("待转换".to_owned(), m.on_surface_variant),
        Status::Running => ("转换中…".to_owned(), m.primary),
        Status::Done(summary) => (summary.text.clone(), m.primary),
        Status::Failed(err) => (format!("失败：{err}"), m.error),
    };
    let status_galley = ui.painter().layout(
        status_text,
        egui::FontId::proportional(12.0),
        status_color,
        max_width,
    );
    ui.painter().galley(
        egui::pos2(text_left, rect.center().y + 3.0),
        status_galley,
        status_color,
    );

    if close_response.hovered() || close_response.is_pointer_button_down_on() {
        ui.painter().circle_filled(
            close.center(),
            16.0,
            egui::Color32::from_rgba_unmultiplied(m.on_surface.r(), m.on_surface.g(), m.on_surface.b(), 22),
        );
    }
    let c = close.center();
    let cross = 5.0;
    let stroke = egui::Stroke::new(1.6, m.on_surface_variant);
    ui.painter().line_segment(
        [
            egui::pos2(c.x - cross, c.y - cross),
            egui::pos2(c.x + cross, c.y + cross),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            egui::pos2(c.x + cross, c.y - cross),
            egui::pos2(c.x - cross, c.y + cross),
        ],
        stroke,
    );
    if close_response.clicked() {
        dispatch.dispatch(Msg::Remove(index));
    } else {
        // 整行（除移除按钮外）都能点选。
        let row = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(close.left() - 4.0, rect.bottom()),
        );
        let row_response =
            ui.interact(row, ui.id().with(("select", index)), egui::Sense::click());
        if row_response.clicked() {
            dispatch.dispatch(Msg::Select(index));
        }
    }
}

/// 设置卡片内容。
fn settings_card(ui: &mut egui::Ui, m: &M3, settings: &Settings, dispatch: &Dispatcher<Msg>) {
    section_title(ui, m, "转换设置", None);

    let mut primary_quality = settings.primary_quality as f32;
    if slider(
        ui,
        m,
        "主图质量",
        &mut primary_quality,
        60.0..=100.0,
        0,
        " %",
    ) {
        dispatch.dispatch(Msg::SetPrimaryQuality(primary_quality.round() as u8));
    }

    let mut gain_quality = settings.gainmap_quality as f32;
    if slider(
        ui,
        m,
        "增益图质量",
        &mut gain_quality,
        50.0..=100.0,
        0,
        " %",
    ) {
        dispatch.dispatch(Msg::SetGainQuality(gain_quality.round() as u8));
    }

    let mut auto_tone = settings.auto_tone;
    if switch(
        ui,
        m,
        "auto_tone",
        "自动推断提亮参数",
        Some("按每张照片自身的亮度分布决定起点与上限"),
        &mut auto_tone,
    ) {
        dispatch.dispatch(Msg::SetAutoTone(auto_tone));
    }

    if !settings.auto_tone {
        let mut max_boost = settings.max_boost;
        if slider(ui, m, "提亮上限", &mut max_boost, 1.0..=6.0, 1, "×") {
            dispatch.dispatch(Msg::SetMaxBoost(max_boost));
        }

        let mut knee = settings.knee;
        if slider(ui, m, "提亮起点", &mut knee, 0.0..=0.95, 2, "") {
            dispatch.dispatch(Msg::SetKnee(knee));
        }
        ui.label(
            egui::RichText::new("越小则越多中间调参与提亮；上限 2–4 最自然")
                .size(11.5)
                .color(m.on_surface_variant),
        );
    }

    ui.add_space(14.0);
    ui.label(
        egui::RichText::new("增益图分辨率")
            .size(14.0)
            .color(m.on_surface),
    );
    ui.add_space(8.0);
    if let Some(scale) = segmented(
        ui,
        m,
        "scale",
        settings.scale,
        &[(1, "全尺寸"), (2, "半尺寸"), (4, "四分之一")],
    ) {
        dispatch.dispatch(Msg::SetScale(scale));
    }

    ui.add_space(8.0);
    let mut progressive = settings.progressive;
    if switch(
        ui,
        m,
        "progressive",
        "渐进式 JPEG",
        Some("先模糊后清晰，体积略大"),
        &mut progressive,
    ) {
        dispatch.dispatch(Msg::SetProgressive(progressive));
    }

    let mut keep_exif = settings.keep_exif;
    if switch(
        ui,
        m,
        "keep_exif",
        "保留原始 EXIF",
        Some("拍摄参数、时间、相机型号"),
        &mut keep_exif,
    ) {
        dispatch.dispatch(Msg::SetKeepExif(keep_exif));
    }

    let mut display_p3 = settings.to_display_p3;
    if switch(
        ui,
        m,
        "display_p3",
        "转换为 Display P3",
        Some("更广色域，需要查看器支持"),
        &mut display_p3,
    ) {
        dispatch.dispatch(Msg::SetDisplayP3(display_p3));
    }
}
