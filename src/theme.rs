//! 全局主题：深浅两套品牌配色 + 统一的间距 / 字号 / 圆角。
//!
//! 只在这里定义颜色与度量，界面层通过 [`palette`] 取语义色，
//! 不再直接使用 `Color32::RED` 之类的固定色。

use eframe::egui::{
    self,
    style::{Selection, WidgetVisuals, Widgets},
    Color32, CornerRadius, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Vec2,
};

/// 控件（按钮 / 输入框 / 下拉）圆角
pub const CONTROL_RADIUS: u8 = 8;
/// 卡片圆角
pub const CARD_RADIUS: u8 = 10;

/// 间距刻度（4 / 8 / 12 / 16）
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
pub const SPACE_MD: f32 = 12.0;
pub const SPACE_LG: f32 = 16.0;

/// 语义色。深色与浅色各一套，保证正文对比度 ≥ 4.5:1。
#[derive(Clone, Copy)]
pub struct Palette {
    /// 窗口底色
    pub bg: Color32,
    /// 卡片 / 顶栏 / 底栏底色
    pub surface: Color32,
    /// 次级面（按钮底、文件行、分段控件轨道）
    pub surface_alt: Color32,
    /// 悬停面
    pub hover: Color32,
    /// 描边
    pub border: Color32,
    /// 悬停描边
    pub border_hover: Color32,
    /// 输入框底色
    pub input: Color32,
    /// 主文本
    pub text: Color32,
    /// 次级文本
    pub text_secondary: Color32,
    /// 品牌强调色
    pub accent: Color32,
    /// 强调色上的文字
    pub accent_ink: Color32,
    /// 强调色的低透明度衬底
    pub accent_soft: Color32,
    /// 错误
    pub danger: Color32,
    /// 警告
    pub warn: Color32,
    /// 成功
    pub ok: Color32,
}

pub fn dark_palette() -> Palette {
    Palette {
        bg: rgb(0x16, 0x18, 0x1B),
        surface: rgb(0x1E, 0x21, 0x26),
        surface_alt: rgb(0x24, 0x28, 0x2E),
        hover: rgb(0x2A, 0x2F, 0x36),
        border: rgb(0x2D, 0x32, 0x3A),
        border_hover: rgb(0x3A, 0x40, 0x48),
        input: rgb(0x15, 0x17, 0x1A),
        text: rgb(0xE8, 0xEA, 0xED),
        text_secondary: rgb(0xA7, 0xAD, 0xB6),
        accent: rgb(0x17, 0xB9, 0x9B),
        accent_ink: rgb(0x04, 0x23, 0x1D),
        accent_soft: rgba(0x17, 0xB9, 0x9B, 33),
        danger: rgb(0xF8, 0x71, 0x71),
        warn: rgb(0xF2, 0xB1, 0x4A),
        ok: rgb(0x4A, 0xDE, 0x80),
    }
}

pub fn light_palette() -> Palette {
    Palette {
        bg: rgb(0xF4, 0xF6, 0xF8),
        surface: rgb(0xFF, 0xFF, 0xFF),
        surface_alt: rgb(0xF1, 0xF3, 0xF6),
        hover: rgb(0xE8, 0xEC, 0xF1),
        border: rgb(0xE2, 0xE6, 0xEB),
        border_hover: rgb(0xCF, 0xD5, 0xDD),
        input: rgb(0xFF, 0xFF, 0xFF),
        text: rgb(0x1D, 0x23, 0x29),
        text_secondary: rgb(0x5B, 0x64, 0x70),
        accent: rgb(0x0C, 0x7F, 0x6C),
        accent_ink: rgb(0xFF, 0xFF, 0xFF),
        accent_soft: rgba(0x0C, 0x7F, 0x6C, 26),
        danger: rgb(0xDC, 0x26, 0x26),
        warn: rgb(0xB4, 0x53, 0x09),
        ok: rgb(0x15, 0x80, 0x3D),
    }
}

/// 取当前实际生效主题的语义色（egui 0.37 默认跟随系统明暗）。
pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        dark_palette()
    } else {
        light_palette()
    }
}

/// 写入两套主题：视觉（颜色 / 圆角）按主题分别配置，度量与字号共用。
pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(Theme::Dark, |style| {
        style.visuals = visuals(dark_palette(), true);
        tune(style);
    });
    ctx.style_mut_of(Theme::Light, |style| {
        style.visuals = visuals(light_palette(), false);
        tune(style);
    });
}

/// 间距 / 字号 / 动效——两套主题一致
fn tune(style: &mut egui::Style) {
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(40.0, 28.0);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.text_edit_width = 280.0;
    style.animation_time = 0.25;

    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(20.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(13.5)),
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Monospace, FontId::monospace(12.0)),
    ]
    .into();
}

fn visuals(p: Palette, dark_mode: bool) -> egui::Visuals {
    let mut v = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.faint_bg_color = p.surface_alt;
    v.extreme_bg_color = p.input;
    v.text_edit_bg_color = Some(p.input);
    v.code_bg_color = p.surface_alt;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.menu_corner_radius = CornerRadius::same(CONTROL_RADIUS);
    v.selection = Selection {
        bg_fill: p.accent_soft,
        stroke: Stroke::new(1.0, p.text),
    };
    // 阴影也按主题给色，避免浅色下出现 egui 默认的深灰投影
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: if dark_mode {
            Color32::from_black_alpha(120)
        } else {
            Color32::from_rgba_unmultiplied(15, 23, 42, 34)
        },
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: if dark_mode {
            Color32::from_black_alpha(90)
        } else {
            Color32::from_rgba_unmultiplied(15, 23, 42, 26)
        },
    };
    v.slider_trailing_fill = true;

    let frame = |bg: Color32, stroke: Color32, fg: Color32, radius: u8| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(radius),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    };

    v.widgets = Widgets {
        noninteractive: frame(p.surface, p.border, p.text, CONTROL_RADIUS),
        inactive: frame(p.surface_alt, p.border, p.text, CONTROL_RADIUS),
        hovered: frame(p.hover, p.border_hover, p.text, CONTROL_RADIUS),
        active: frame(p.hover, p.accent, p.text, CONTROL_RADIUS),
        open: frame(p.hover, p.border, p.text, CONTROL_RADIUS),
    };

    v
}

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}
