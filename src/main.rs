//! JPG → Ultra HDR：把普通 SDR JPEG 转成带增益图的 Ultra HDR JPEG。
//!
//! Ultra HDR 文件本身仍是一张普通 JPEG（primary），额外附带一张增益图，
//! 支持 HDR 的查看器按增益图把画面提亮，不支持的查看器则照常显示 SDR 版本。

// 发布版不打开控制台窗口；debug 版保留控制台，方便看 stderr 诊断信息。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;
use jpg2ultrahdr::{Report, Settings, convert as convert_one, output_path};
use modern_egui::theme::{self, UiButtons, UiMetrics, UiPanels, UiText, ui_ext::PanelVariant};

mod config;

/// 设置改动后最快多久写一次盘，避免拖滑块时每帧都碰磁盘。
const WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

fn main() -> eframe::Result {
    // 支持把文件/文件夹拖到 exe 图标上直接打开。
    let initial: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    // 上次的窗口尺寸 / 主题 / 参数，没有配置文件时全是默认值。
    let persisted = config::load();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("JPG → Ultra HDR")
        .with_min_inner_size([900.0, 560.0]);
    viewport = match persisted.window_size {
        Some((width, height)) => viewport.with_inner_size([width, height]),
        None => viewport.with_inner_size([1180.0, 760.0]),
    };
    if persisted.maximized {
        viewport = viewport.with_maximized(true);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "jpg2ultrahdr",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx);
            cc.egui_ctx.set_theme(if persisted.dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let mut app = App::new(cc.egui_ctx.clone(), persisted);
            for path in initial {
                if path.is_dir() {
                    app.add_folder(path);
                } else {
                    app.add_files([path]);
                }
            }
            if std::env::var_os("JPG2ULTRAHDR_AUTOSTART").is_some() {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}

/// egui 自带字体没有中日韩字形；补一个系统中文字体作后备。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 4] = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return;
    }
}

enum Status {
    Pending,
    Running,
    Done(Report),
    Failed(String),
}

struct Entry {
    path: PathBuf,
    status: Status,
}

enum Event {
    Started(usize),
    Done(usize, Result<Report, String>),
    Finished,
}

struct Preview {
    texture: egui::TextureHandle,
    path: PathBuf,
}

struct App {
    ctx: egui::Context,
    entries: Vec<Entry>,
    settings: Settings,
    out_dir: Option<PathBuf>,
    running: bool,
    total: usize,
    finished: usize,
    preview: Option<Preview>,
    preview_error: Option<String>,
    notice: Option<String>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// `true` = 深色主题。
    dark: bool,
    /// 非最大化时的窗口逻辑尺寸，随窗口拖动实时更新。
    window_size: Option<(f32, f32)>,
    maximized: bool,
    /// 上次写盘时的配置文本，用来判断要不要重新保存。
    saved_config: String,
    /// 上次写盘的时刻，用于限制拖滑块时的写盘频率。
    last_write: Option<std::time::Instant>,
}

impl App {
    fn new(ctx: egui::Context, persisted: config::Persisted) -> Self {
        let (tx, rx) = channel();
        Self {
            ctx,
            entries: Vec::new(),
            settings: persisted.settings,
            out_dir: persisted.out_dir,
            running: false,
            total: 0,
            finished: 0,
            preview: None,
            preview_error: None,
            notice: None,
            tx,
            rx,
            dark: persisted.dark,
            window_size: persisted.window_size,
            maximized: persisted.maximized,
            // 留空，让第一帧无条件写一次配置文件：这样即使从没改过设置，
            // 窗口尺寸和主题也会被记住。
            saved_config: String::new(),
            last_write: None,
        }
    }

    fn add_files(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if !is_jpeg(&path) || self.entries.iter().any(|e| e.path == path) {
                continue;
            }
            self.entries.push(Entry {
                path,
                status: Status::Pending,
            });
        }
        if self.preview.is_none() {
            if let Some(first) = self.entries.first().map(|e| e.path.clone()) {
                self.show_preview(&first);
            }
        }
    }

    fn add_folder(&mut self, dir: PathBuf) {
        let mut found = Vec::new();
        collect_jpegs(&dir, 0, &mut found);
        found.sort();
        self.add_files(found);
    }

    fn show_preview(&mut self, path: &Path) {
        self.preview_error = None;
        match preview_texture(&self.ctx, path) {
            Ok(texture) => {
                self.preview = Some(Preview {
                    texture,
                    path: path.to_path_buf(),
                })
            }
            Err(err) => {
                self.preview = None;
                self.preview_error = Some(err);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.preview = None;
        self.preview_error = None;
        self.notice = None;
        self.finished = 0;
        self.total = 0;
    }

    /// 主题切换只改偏好，两套调色板在 `theme::apply` 时就已经装好了。
    fn apply_theme(&self) {
        self.ctx.set_theme(if self.dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
    }

    /// 把当前设置、输出目录、主题、窗口尺寸写回配置文件。
    ///
    /// 拖动滑块时每帧都会变，所以最多每 [`WRITE_INTERVAL`] 写一次；
    /// 退出时再由 [`Self::flush_config`] 补最后一次。
    fn persist(&mut self) {
        // 窗口状态每帧都从后端读；最大化时记录的是还原后的尺寸。
        self.ctx.input(|i| {
            let viewport = i.viewport();
            if let Some(maximized) = viewport.maximized {
                self.maximized = maximized;
            }
            if !self.maximized {
                if let Some(rect) = viewport.inner_rect {
                    self.window_size = Some((rect.width(), rect.height()));
                }
            }
        });

        if config::render(&self.snapshot()) == self.saved_config {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(last) = self.last_write {
            let elapsed = now.duration_since(last);
            if elapsed < WRITE_INTERVAL {
                // 还没到时间，下一帧再来。
                self.ctx.request_repaint_after(WRITE_INTERVAL - elapsed);
                return;
            }
        }
        self.flush_config();
    }

    fn snapshot(&self) -> config::Persisted {
        config::Persisted {
            settings: self.settings.clone(),
            out_dir: self.out_dir.clone(),
            dark: self.dark,
            window_size: self.window_size,
            maximized: self.maximized,
        }
    }

    /// 立即落盘，不管写盘间隔。退出时调用。
    fn flush_config(&mut self) {
        let persisted = self.snapshot();
        let text = config::render(&persisted);
        if text != self.saved_config {
            config::save(&persisted);
            self.saved_config = text;
        }
        self.last_write = Some(std::time::Instant::now());
    }

    fn theme_toggle(&mut self, ui: &mut egui::Ui) {
        use modern_egui::theme::{Palette, metrics};

        // 太阳/月亮用自己画的图标：egui 自带字体没有 ☀ / ☾ 字形，
        // 直接用字符会显示成方框。
        let palette = Palette::of(ui.ctx());
        let size = egui::vec2(metrics::CONTROL_HEIGHT, metrics::CONTROL_HEIGHT);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        let response = response
            .on_hover_text(if self.dark {
                "切换到浅色主题"
            } else {
                "切换到深色主题"
            })
            .on_hover_cursor(egui::CursorIcon::PointingHand);

        let painter = ui.painter();
        let background = if response.hovered() {
            painter.rect_filled(rect, metrics::RADIUS_SM, palette.surface_alt);
            palette.surface_alt
        } else {
            palette.bg
        };        let color = if response.hovered() {
            palette.text_strong
        } else {
            palette.text_muted
        };

        let center = rect.center();
        let radius = rect.height() * 0.22;
        if self.dark {
            // 月亮：实心圆再挖掉一个偏移的圆。
            painter.circle_filled(center, radius, color);
            painter.circle_filled(
                center + egui::vec2(radius * 0.66, -radius * 0.56),
                radius * 0.94,
                background,
            );
        } else {
            // 太阳：实心圆 + 八条射线。
            painter.circle_filled(center, radius * 0.62, color);
            for step in 0..8 {
                let angle = std::f32::consts::TAU * (step as f32) / 8.0;
                let dir = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [
                        center + dir * (radius * 1.0),
                        center + dir * (radius * 1.5),
                    ],
                    egui::Stroke::new(1.4, color),
                );
            }
        }

        if response.clicked() {
            self.dark = !self.dark;
            self.apply_theme();
            self.flush_config();
        }
    }

    fn start(&mut self) {
        if self.running || self.entries.is_empty() {
            return;
        }
        for entry in &mut self.entries {
            entry.status = Status::Pending;
        }
        self.running = true;
        self.total = self.entries.len();
        self.finished = 0;
        self.notice = None;

        let jobs: Vec<(usize, PathBuf)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.path.clone()))
            .collect();
        let settings = self.settings.clone();
        let out_dir = self.out_dir.clone();
        let tx = self.tx.clone();

        std::thread::spawn(move || {
            // 一张 2000 万像素的照片在解码后会占约 150 MB，所以并发数要按住
            // 内存来定，而不是按 CPU 核数。每个任务内部已经用 rayon 并行。
            let workers = concurrency();
            let next = std::sync::atomic::AtomicUsize::new(0);
            let next = &next;
            let jobs = &jobs;
            let tx = &tx;
            let settings = &settings;
            let out_dir = &out_dir;

            std::thread::scope(|scope| {
                for _ in 0..workers.min(jobs.len().max(1)) {
                    scope.spawn(move || {
                        loop {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((slot, path)) = jobs.get(index) else {
                                break;
                            };
                            let _ = tx.send(Event::Started(*slot));
                            let dst = output_path(path, out_dir.as_deref());
                            let result =
                                convert_one(path, &dst, settings).map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Event::Done(*slot, result));
                        }
                    });
                }
            });

            let _ = tx.send(Event::Finished);
        });
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Started(index) => {
                    if let Some(entry) = self.entries.get_mut(index) {
                        entry.status = Status::Running;
                    }
                }
                Event::Done(index, result) => {
                    self.finished += 1;
                    if let Some(entry) = self.entries.get_mut(index) {
                        entry.status = match result {
                            Ok(report) => Status::Done(report),
                            Err(err) => Status::Failed(err),
                        };
                    }
                }
                Event::Finished => {
                    self.running = false;
                    self.notice = Some(format!("转换完成：{} 个文件", self.finished));
                    if std::env::var_os("JPG2ULTRAHDR_QUIT_WHEN_DONE").is_some() {
                        eprintln!("全部完成：{} 个文件", self.finished);
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for handle in dropped {
            let path = handle.path().to_path_buf();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
        if !files.is_empty() {
            self.add_files(files);
        }
        for dir in dirs {
            self.add_folder(dir);
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("JPEG 图像", &["jpg", "jpeg", "jpe", "jfif"])
            .set_title("选择要转换的 JPEG")
            .pick_files()
        {
            self.add_files(paths);
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("选择包含 JPEG 的文件夹")
            .pick_folder()
        {
            self.add_folder(dir);
        }
    }

    fn pick_out_dir(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("选择输出文件夹")
            .pick_folder()
        {
            self.out_dir = Some(dir);
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.section_title("转换设置");
        ui.space_inline();

        // —— 图像质量 + 提亮曲线合并，减少滚动 ——
        let full = ui.available_width();
        ui.card(|ui| {
            ui.set_min_width(full - 30.0);
            ui.card_title("画质与提亮");
            ui.space_group();

            slider_row(
                ui,
                "主图质量",
                &mut self.settings.primary_quality,
                60..=100,
                " %",
                0,
            );
            slider_row(
                ui,
                "增益图质量",
                &mut self.settings.gainmap_quality,
                50..=100,
                " %",
                0,
            );

            ui.horizontal(|ui| {
                ui.add_sized([92.0, 18.0], egui::Label::new("增益图分辨率"));
                egui::ComboBox::from_id_salt("scale")
                    .selected_text(scale_label(self.settings.scale))
                    .width(ui.available_width() - 12.0)
                    .show_ui(ui, |ui| {
                        for scale in [1u32, 2, 4] {
                            ui.selectable_value(
                                &mut self.settings.scale,
                                scale,
                                scale_label(scale),
                            );
                        }
                    });
            });

            ui.space_group();
            slider_row_f32(ui, "提亮上限", &mut self.settings.max_boost, 1.0..=8.0, "×", 1);
            slider_row_f32(ui, "提亮起点", &mut self.settings.knee, 0.0..=0.95, "", 2);
            ui.metadata_label("越小则越多中间调参与提亮；上限 2–4 最自然");
        });

        ui.space_group();

        // —— 输出选项 ——
        let full = ui.available_width();
        ui.card(|ui| {
            ui.set_min_width(full - 30.0);
            ui.card_title("输出选项");
            ui.space_group();

            ui.checkbox(&mut self.settings.progressive, "渐进式 JPEG");
            ui.checkbox(&mut self.settings.keep_exif, "保留原始 EXIF");
            ui.checkbox(&mut self.settings.to_display_p3, "转换为 Display P3");
        });

        ui.space_group();

        // —— 输出位置 ——
        let full = ui.available_width();
        ui.card_with_variant(PanelVariant::SurfaceAlt, |ui| {
            ui.set_min_width(full - 30.0);
            ui.card_title("输出位置");
            ui.space_group();

            let text = self
                .out_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "与源文件相同目录".to_owned());
            ui.add(egui::Label::new(egui::RichText::new(text).small()).truncate());
            ui.space_group();
            ui.horizontal(|ui| {
                if ui.ghost_button("选择目录…").clicked() {
                    self.pick_out_dir();
                }
                if self.out_dir.is_some() && ui.ghost_button("默认").clicked() {
                    self.out_dir = None;
                }
            });
        });
    }

    /// 返回被点击的文件下标（预览切换要等循环结束再做）。
    fn file_list(&mut self, ui: &mut egui::Ui) {
        if self.entries.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.muted_label("还没有添加文件");
                ui.space_group();
                ui.metadata_label("把 JPEG 拖进窗口，或点底部“添加文件”");
            });
            return;
        }

        let mut clicked = None;
        let mut remove = None;
        let selected_path = self.preview.as_ref().map(|p| p.path.clone());

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, entry) in self.entries.iter().enumerate() {
                    let selected = selected_path.as_ref() == Some(&entry.path);
                    let name = entry
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();

                    let palette = modern_egui::theme::Palette::of(ui.ctx());
                    let fill = if selected {
                        palette.surface_alt
                    } else {
                        palette.surface
                    };
                    let stroke = if selected {
                        egui::Stroke::new(1.0, palette.accent)
                    } else {
                        egui::Stroke::new(1.0, palette.border)
                    };

                    let response = egui::Frame::new()
                        .fill(fill)
                        .stroke(stroke)
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    let text = if selected {
                                        egui::RichText::new(&name).strong().color(palette.accent)
                                    } else {
                                        egui::RichText::new(&name).strong()
                                    };
                                    if ui.selectable_label(false, text).clicked() {
                                        clicked = Some(index);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if !self.running && selected {
                                                if ui.small_button("移除").clicked() {
                                                    remove = Some(index);
                                                }
                                            }
                                        },
                                    );
                                });
                                ui.add_space(2.0);
                                status_badge(ui, &entry.status, palette);
                            });
                        })
                        .response;

                    // 整块可点：点卡片任意位置都切换预览。
                    let response = response
                        .interact(egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if response.clicked() && remove.is_none() {
                        clicked = Some(index);
                    }

                    ui.add_space(6.0);
                }
            });

        // 移除优先：先移除再决定是否切预览，避免下标错位。
        if let Some(index) = remove {
            self.entries.remove(index);
            clicked = None;
            if self
                .preview
                .as_ref()
                .is_some_and(|p| !self.entries.iter().any(|e| e.path == p.path))
            {
                self.preview = None;
            }
        }
        if let Some(index) = clicked {
            if let Some(path) = self.entries.get(index).map(|e| e.path.clone()) {
                self.show_preview(&path);
            }
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        match &self.preview {
            Some(preview) => {
                let available = ui.available_size();
                let texture_size = preview.texture.size_vec2();
                let scale = (available.x / texture_size.x)
                    .min(available.y / texture_size.y)
                    .min(1.0)
                    .max(0.05);
                let shown = texture_size * scale;

                // 居中放置预览图。
                let pad_x = ((available.x - shown.x) * 0.5).max(0.0);
                let pad_y = ((available.y - shown.y) * 0.5).max(0.0) - 12.0;
                ui.add_space(pad_y.max(0.0));
                ui.horizontal(|ui| {
                    ui.add_space(pad_x);
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::new(
                            preview.texture.id(),
                            shown,
                        ))
                        .corner_radius(6.0),
                    );
                });
            }
            None => {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    if let Some(err) = &self.preview_error {
                        ui.colored_label(ui.visuals().error_fg_color, err.as_str());
                    } else {
                        ui.muted_label("选择左侧文件以预览");
                    }
                });
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();
        self.handle_drops(&ctx);
        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        debug_screenshot(&ctx);

        // 面板顺序：顶栏 → 底栏 → 右侧设置 → 左侧列表 → 中央预览。
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.app_title("JPG → Ultra HDR");
                ui.add_space(10.0);
                ui.muted_label("把普通 JPEG 变成带增益图的 HDR 照片");

                // 右侧区域用固定宽度占位，避免 ProgressBar 被 RTL 布局压扁。
                let right = (ui.available_width() - 300.0).max(0.0);
                ui.add_space(right);
                if self.running {
                    let progress = if self.total == 0 {
                        0.0
                    } else {
                        self.finished as f32 / self.total as f32
                    };
                    progress_chip(
                        ui,
                        progress,
                        &format!("{} / {}", self.finished, self.total),
                    );
                } else {
                    ui.add_enabled_ui(!self.entries.is_empty(), |ui| {
                        if ui.primary_button("开始转换").clicked() {
                            self.start();
                        }
                    });
                }
                ui.add_space(6.0);
                self.theme_toggle(ui);
            });
            ui.add_space(10.0);
        });

        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.add_enabled_ui(!self.running, |ui| {
                    if ui.ghost_button("添加文件…").clicked() {
                        self.pick_files();
                    }
                    if ui.ghost_button("添加文件夹…").clicked() {
                        self.pick_folder();
                    }
                    if !self.entries.is_empty() && ui.ghost_button("清空").clicked() {
                        self.clear();
                    }
                });
                if let Some(notice) = self.notice.clone() {
                    ui.separator();
                    ui.muted_label(notice);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(4.0);
                    ui.metadata_label(format!("{} 个文件", self.entries.len()));
                });
            });
            ui.add_space(7.0);
        });

        egui::Panel::right("settings_panel")
            .resizable(true)
            .default_size(300.0)
            .min_size(260.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.settings_panel(ui));
            });

        egui::Panel::left("files_panel")
            .resizable(true)
            .default_size(330.0)
            .min_size(240.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                self.file_list(ui);
            });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.preview_panel(ui);
        });

        // 所有控件都画完之后再判断配置有没有改动。
        self.persist();
    }

    /// 关窗时会调用，把还没落盘的改动补写一次。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_config();
    }
}

/// 一行式滑块：左侧标签固定宽度，右侧滑块占满剩余空间。
fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u8,
    range: std::ops::RangeInclusive<u8>,
    suffix: &str,
    decimals: usize,
) {
    ui.horizontal(|ui| {
        ui.add_sized([92.0, 18.0], egui::Label::new(label));
        ui.add(
            egui::Slider::new(value, range)
                .suffix(suffix)
                .fixed_decimals(decimals),
        );
    });
}

fn slider_row_f32(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    decimals: usize,
) {
    ui.horizontal(|ui| {
        ui.add_sized([92.0, 18.0], egui::Label::new(label));
        ui.add(
            egui::Slider::new(value, range)
                .suffix(suffix)
                .fixed_decimals(decimals)
                .clamping(egui::SliderClamping::Always),
        );
    });
}

/// 自绘进度条：`egui::ProgressBar` 在 progress=0 时会画出宽度等于高度的
/// 圆角"最小填充"，看起来像个蓝点，这里避开这个问题。
fn progress_chip(ui: &mut egui::Ui, progress: f32, label: &str) {
    use modern_egui::theme::{Palette, metrics};

    let palette = Palette::of(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(220.0, metrics::CONTROL_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let radius = rect.height() * 0.5;

    painter.rect_filled(rect, radius, palette.surface_alt);

    if progress > 0.0 {
        let mut filled = rect;
        filled.set_width((rect.width() * progress.clamp(0.0, 1.0)).max(rect.height()));
        painter.rect_filled(filled, radius, palette.accent);
    }

    // 文字永远画在右侧内边距处，避让填充条本身。
    painter.text(
        egui::pos2(rect.right() - 10.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        label,
        egui::FontId::proportional(metrics::FONT_SM),
        if progress > 0.85 {
            palette.on_accent
        } else {
            palette.text
        },
    );
}

fn status_badge(ui: &mut egui::Ui, status: &Status, palette: modern_egui::theme::Palette) {
    use modern_egui::theme::TextColor;

    match status {
        Status::Pending => {
            ui.text_colored("待转换", modern_egui::theme::TextSize::Xs, TextColor::Muted);
        }
        Status::Running => {
            ui.text_colored("转换中…", modern_egui::theme::TextSize::Xs, TextColor::Accent);
        }
        Status::Done(report) => {
            let kb = report.bytes as f64 / 1024.0;
            let size = if kb >= 1024.0 {
                format!("{:.1} MB", kb / 1024.0)
            } else {
                format!("{kb:.0} KB")
            };
            ui.text_colored(
                format!(
                    "{}×{} · 增益图 {}×{} · 峰值 {:.1}× · {}",
                    report.width,
                    report.height,
                    report.gain_width,
                    report.gain_height,
                    report.peak_boost,
                    size
                ),
                modern_egui::theme::TextSize::Xs,
                TextColor::Success,
            );
        }
        Status::Failed(err) => {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("失败：{err}"))
                        .size(modern_egui::theme::metrics::FONT_XS)
                        .color(palette.danger),
                )
                .wrap(),
            );
        }
    }
}

fn scale_label(scale: u32) -> &'static str {    match scale {
        1 => "全分辨率（最大，最清晰）",
        2 => "半分辨率（推荐）",
        _ => "四分之一（最小）",
    }
}

fn is_jpeg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "jpe" | "jfif")
    )
}

/// 开发期自检截图交给 eframe 自带的 `__screenshot` feature
/// （环境变量 `EFRAME_SCREENSHOT_TO=<path>.png`），这里不再手写。
fn debug_screenshot(_ctx: &egui::Context) {}

/// 同时处理多少个文件：按可用内存估算，一张 2000 万像素的照片约占 150 MB。
fn concurrency() -> usize {    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let gib = total_memory_gib().unwrap_or(8.0);
    // 保留一半内存给系统与解码峰值，其余按每任务 200 MB 估算。
    let budget = ((gib - 2.0).max(1.0) * 1024.0 / 200.0) as usize;
    cores.min(budget).clamp(1, 8)
}

fn total_memory_gib() -> Option<f64> {
    #[cfg(target_os = "windows")]
    {
        use std::sync::OnceLock;
        static TOTAL: OnceLock<Option<f64>> = OnceLock::new();
        *TOTAL.get_or_init(|| {
            // GlobalMemoryStatusEx
            #[repr(C)]
            struct MemoryStatusEx {
                length: u32,
                memory_load: u32,
                total_phys: u64,
                avail_phys: u64,
                total_page_file: u64,
                avail_page_file: u64,
                total_virtual: u64,
                avail_virtual: u64,
                avail_extended_virtual: u64,
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
            }
            let mut status = MemoryStatusEx {
                length: std::mem::size_of::<MemoryStatusEx>() as u32,
                memory_load: 0,
                total_phys: 0,
                avail_phys: 0,
                total_page_file: 0,
                avail_page_file: 0,
                total_virtual: 0,
                avail_virtual: 0,
                avail_extended_virtual: 0,
            };
            // SAFETY: `status` is a correctly sized, properly aligned
            // `MEMORYSTATUSEX` and `length` is set to its size, as the API
            // requires.
            let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
            if ok == 0 {
                None
            } else {
                Some(status.total_phys as f64 / 1024.0 / 1024.0 / 1024.0)
            }
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

fn collect_jpegs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {    if depth > 6 {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jpegs(&path, depth + 1, out);
        } else if is_jpeg(&path) {
            out.push(path);
        }
    }
}

fn preview_texture(ctx: &egui::Context, path: &Path) -> Result<egui::TextureHandle, String> {
    let image = image::open(path).map_err(|e| format!("无法预览：{e}"))?;
    let small = image.thumbnail(700, 700).to_rgba8();
    let size = [small.width() as usize, small.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, small.as_raw());
    Ok(ctx.load_texture("preview", color, egui::TextureOptions::LINEAR))
}

