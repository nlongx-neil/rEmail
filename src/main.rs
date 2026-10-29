#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod logger;
mod progress;
mod sender;
mod sevenz;

use app::MailApp;
use eframe::{egui};
use std::fs;

/// Windows + egui 主入口
fn main() -> Result<(), eframe::Error> {
    let mut options = eframe::NativeOptions::default();
    options.centered = true;

    eframe::run_native(
        "Mail Sender",
        options,
        Box::new(|cc| {
            // 使用微软雅黑，避免中文乱码
            setup_font(&cc.egui_ctx);

            Box::new(MailApp::default())
        }),
    )
}

/// 注册 msyh.ttc 字体
fn setup_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 尝试系统字体（按优先级）
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\STSONG.TTF",
        r"C:\Windows\Fonts\simfang.ttf",
    ];

    let mut loaded = false;

    for path in candidates {
        if let Ok(data) = fs::read(path) {
            fonts
                .font_data
                .insert("system_cn".into(), egui::FontData::from_owned(data));
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
                    .insert("font".to_owned(), egui::FontData::from_owned(data));

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
    // 从 exe 同目录加载 msyh.ttc

    ctx.set_fonts(fonts);
}
