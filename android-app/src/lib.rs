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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;

use egui_android_framework::prelude::*;
use egui_android_framework::runtime::{RuntimeConfig, RuntimeContext, UiNotifier};
use egui_android_framework::ui::theme::{MaterialTheme, Theme};

use jpg2ultrahdr::{Report, Settings, convert as convert_one, output_path};

mod design;
use design::{
    ButtonKind, M3, button, card, linear_progress, section_title, segmented, slider, switch,
    top_bar,
};

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
    let dst = output_path(src, Some(out_dir()));
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
    SetMaxBoost(f32),
    SetKnee(f32),
    SetProgressive(bool),
    SetDisplayP3(bool),
    SetKeepExif(bool),
    Remove(usize),
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
        }
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
        egui::CentralPanel::no_frame().show(ui, |ui| {
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
                                file_row(ui, &m, index, item, dispatch);
                            }
                        });
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
            Msg::SetMaxBoost(v) => self.settings.max_boost = v,
            Msg::SetKnee(v) => self.settings.knee = v,
            Msg::SetProgressive(v) => self.settings.progressive = v,
            Msg::SetDisplayP3(v) => self.settings.to_display_p3 = v,
            Msg::SetKeepExif(v) => self.settings.keep_exif = v,
            Msg::Remove(index) => {
                if index < self.items.len() {
                    self.items.remove(index);
                    self.notice = format!("还剩 {} 张图片", self.items.len());
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
fn file_row(
    ui: &mut egui::Ui,
    m: &M3,
    index: usize,
    item: &Item,
    dispatch: &Dispatcher<Msg>,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 60.0), egui::Sense::hover());
    let painter = ui.painter();

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
