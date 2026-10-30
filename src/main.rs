#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autofill;
mod config;
mod logger;
mod progress;
mod schedule;
mod sender;
mod sevenz;

use app::MailApp;
use eframe::egui;
use std::fs;

/// 桌面入口（Windows / macOS 通用）
fn main() -> Result<(), eframe::Error> {
    // 窗口图标（128x128 PNG，来自 assets/，构建时嵌入二进制）
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Mail Sender")
        // 宽度为原值 1100 / 900 的 60%（660 / 540）；高度保持 760 / 600
        .with_inner_size([660.0, 760.0])
        .with_min_inner_size([540.0, 600.0]);
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        // 0.37 默认渲染后端改为 wgpu，这里显式指定 glow（OpenGL），
        // 保持与 0.27 时期相同的渲染路径，避免老机器驱动差异
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "Mail Sender",
        options,
        Box::new(|cc| {
            // 使用系统中文字体，避免中文乱码
            setup_font(&cc.egui_ctx);

            Ok(Box::new(MailApp::default()) as Box<dyn eframe::App>)
        }),
    )
}

/// 从内嵌 PNG 解码窗口图标；失败时返回 None（不影响启动）
fn load_app_icon() -> Option<egui::IconData> {
    const ICON_PNG: &[u8] = include_bytes!("../assets/icon_128.png");

    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => Some(icon),
        Err(e) => {
            eprintln!("加载窗口图标失败: {e}");
            None
        }
    }
}

/// 注册中文字体（Windows 使用系统字体，macOS 使用 PingFang 等系统字体）
fn setup_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 尝试系统字体（按优先级）
    #[cfg(windows)]
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\STSONG.TTF",
        r"C:\Windows\Fonts\simfang.ttf",
    ];
    #[cfg(target_os = "macos")]
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
    ];
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let candidates = [
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];

    let mut loaded = false;

    for path in candidates {
        if let Ok(data) = fs::read(path) {
            fonts
                .font_data
                .insert("system_cn".into(), egui::FontData::from_owned(data).into());
            loaded = true;
            break;
        }
    }

    if loaded {
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "system_cn".into());

        fonts
            .families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .push("system_cn".into());
    } else {
        let font_data = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("font.ttf")));

        if let Some(path) = font_data {
            if let Ok(data) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("font".to_owned(), egui::FontData::from_owned(data).into());

                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "font".to_owned());

                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("font".to_owned());
            }
        }
    }

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_window_icon_decodes() {
        let icon = super::load_app_icon().expect("assets/icon_128.png 应能解码为窗口图标");
        assert_eq!(icon.width, 128);
        assert_eq!(icon.height, 128);
        assert_eq!(icon.rgba.len(), 128 * 128 * 4);
    }
}
