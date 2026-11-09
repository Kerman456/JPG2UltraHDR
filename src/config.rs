//! 记住上次使用的参数。
//!
//! 配置写进 `%APPDATA%\jpg2ultrahdr\settings.conf`，纯文本 `key=value` 每行一条，
//! 方便手工查看和删除。删除该文件即恢复全部默认值。

use std::path::PathBuf;

use jpg2ultrahdr::Settings;

/// 需要跨会话保留的全部状态。
pub struct Persisted {
    pub settings: Settings,
    /// `None` 表示输出到源文件所在目录。
    pub out_dir: Option<PathBuf>,
    /// `true` = 深色主题，`false` = 浅色主题。
    pub dark: bool,
    /// 上次退出时的窗口逻辑尺寸（像素）。最大化时记录的是还原后的尺寸。
    pub window_size: Option<(f32, f32)>,
    pub maximized: bool,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            out_dir: None,
            dark: true,
            window_size: None,
            maximized: false,
        }
    }
}

pub fn config_path() -> PathBuf {
    let dir = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("jpg2ultrahdr"))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("settings.conf")
}

pub fn load() -> Persisted {
    let mut persisted = Persisted::default();
    let Ok(text) = std::fs::read_to_string(config_path()) else {
        return persisted;
    };

    let settings = &mut persisted.settings;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "primary_quality" => {
                if let Some(v) = parse_u8(value) {
                    settings.primary_quality = v.clamp(1, 100);
                }
            }
            "gainmap_quality" => {
                if let Some(v) = parse_u8(value) {
                    settings.gainmap_quality = v.clamp(1, 100);
                }
            }
            "scale" => {
                if let Some(v) = parse_u8(value) {
                    // 只接受三种分辨率，其他数值视为损坏。
                    if matches!(v, 1 | 2 | 4) {
                        settings.scale = u32::from(v);
                    }
                }
            }
            "max_boost" => {
                if let Some(v) = parse_f32(value) {
                    settings.max_boost = v.clamp(1.0, 8.0);
                }
            }
            "knee" => {
                if let Some(v) = parse_f32(value) {
                    settings.knee = v.clamp(0.0, 0.95);
                }
            }
            "progressive" => settings.progressive = parse_bool(value).unwrap_or(true),
            "display_p3" => settings.to_display_p3 = parse_bool(value).unwrap_or(false),
            "keep_exif" => settings.keep_exif = parse_bool(value).unwrap_or(true),
            "out_dir" => {
                let path = PathBuf::from(value);
                // 目录可能已经被删掉或移走，这种情况按“默认”处理。
                persisted.out_dir = path.is_dir().then_some(path);
            }
            "dark" => persisted.dark = parse_bool(value).unwrap_or(true),
            "window_width" => {
                if let Some(v) = parse_f32(value) {
                    let width = v.clamp(640.0, 20000.0);
                    persisted.window_size = Some((
                        width,
                        persisted.window_size.map_or(760.0, |(_, h)| h),
                    ));
                }
            }
            "window_height" => {
                if let Some(v) = parse_f32(value) {
                    let height = v.clamp(480.0, 20000.0);
                    persisted.window_size = Some((
                        persisted.window_size.map_or(1180.0, |(w, _)| w),
                        height,
                    ));
                }
            }
            "maximized" => persisted.maximized = parse_bool(value).unwrap_or(false),
            _ => {}
        }
    }
    persisted
}

pub fn save(persisted: &Persisted) {
    let path = config_path();
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // 先写临时文件再改名，避免写到一半断电留下残缺配置。
    let temp = path.with_extension("conf.tmp");
    if std::fs::write(&temp, render(persisted)).is_err() {
        return;
    }
    let _ = std::fs::rename(&temp, &path);
}

/// 序列化成可以直接比较的文本：两帧之间文本不变就说明用户没动过设置。
pub fn render(persisted: &Persisted) -> String {
    let s = &persisted.settings;
    let mut out = String::with_capacity(512);
    out.push_str("# JPG → Ultra HDR 设置，删除本文件即恢复默认值。\n");
    out.push_str("version=1\n");
    out.push_str(&format!("primary_quality={}\n", s.primary_quality));
    out.push_str(&format!("gainmap_quality={}\n", s.gainmap_quality));
    out.push_str(&format!("scale={}\n", s.scale));
    out.push_str(&format!("max_boost={}\n", s.max_boost));
    out.push_str(&format!("knee={}\n", s.knee));
    out.push_str(&format!("progressive={}\n", s.progressive));
    out.push_str(&format!("display_p3={}\n", s.to_display_p3));
    out.push_str(&format!("keep_exif={}\n", s.keep_exif));
    out.push_str(&format!("dark={}\n", persisted.dark));
    out.push_str(&format!("maximized={}\n", persisted.maximized));
    if let Some((width, height)) = persisted.window_size {
        out.push_str(&format!("window_width={width:.0}\n"));
        out.push_str(&format!("window_height={height:.0}\n"));
    }
    if let Some(dir) = &persisted.out_dir {
        out.push_str(&format!("out_dir={}\n", dir.display()));
    }
    out
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_u8(value: &str) -> Option<u8> {
    // 兼容 `92 %` 这类带单位的残留写法。
    value
        .split_whitespace()
        .next()
        .and_then(|token| token.parse().ok())
}

fn parse_f32(value: &str) -> Option<f32> {
    let parsed: f32 = value.split_whitespace().next()?.parse().ok()?;
    parsed.is_finite().then_some(parsed)
}
