use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use eframe::egui;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::autofill;
use crate::config::{AppConfig, SmtpAccount};
use crate::logger::{LogLevel, LogItem, Logger};
use crate::progress::SendProgress;
use crate::schedule;
use crate::sender::start_send_task;
use crate::theme;

/// 主窗口底部状态条高度（只放一行：最新日志 + 计数 + 打开日志按钮）
const STATUS_BAR_HEIGHT: f32 = 30.0;
/// 顶栏高度（品牌区 + 分段控件）
const TOP_BAR_HEIGHT: f32 = 56.0;
/// 顶栏分段控件宽度
const TAB_SWITCH_WIDTH: f32 = 148.0;

/// 日志窗口初始尺寸 / 最小尺寸
const LOG_WINDOW_SIZE: [f32; 2] = [720.0, 520.0];
const LOG_WINDOW_MIN_SIZE: [f32; 2] = [480.0, 320.0];
/// 主窗口窄于此宽度时，日志改为内嵌页（不弹第二个 OS 窗口）
const LOG_INLINE_MAX_WIDTH: f32 = 760.0;

/// 日志等级筛选（日志窗口顶部）
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum LogFilter {
    /// 全部
    #[default]
    All,
    /// 仅 WARN 及以上
    Warn,
    /// 仅 ERROR
    Error,
}

impl LogFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Warn => "警告+",
            Self::Error => "仅错误",
        }
    }

    fn accepts(self, level: LogLevel) -> bool {
        match self {
            Self::All => true,
            Self::Warn => level != LogLevel::Info,
            Self::Error => level == LogLevel::Error,
        }
    }
}

/// 日志视图的滚动跟随状态（主窗口状态条与日志窗口共用同一份）
#[derive(Default)]
struct LogUiState {
    /// 是否自动跟随到日志底部（用户上滚/选中文本时暂停）
    follow: bool,
    /// 上一帧日志条数（判断是否出现新内容）
    seen: usize,
    /// 上一帧日志滚动偏移（判断是否用户手动滚动）
    last_offset: f32,
    /// 上一帧日志滚动区可视区域（判断鼠标是否在日志区内操作）
    view_rect: Option<egui::Rect>,
    /// 是否请求「回到底部」一次性跳转
    jump: bool,
    /// 等级筛选
    filter: LogFilter,
}

impl LogUiState {
    fn new() -> Self {
        Self {
            follow: true,
            filter: LogFilter::All,
            ..Default::default()
        }
    }
}

/// ETA 估算：按已发送分卷的平均耗时推算剩余分卷
#[derive(Default)]
struct EtaTracker {
    start: Option<Instant>,
    /// 上一个文件索引
    file: u64,
    /// 上一个文件已发送分卷数
    sent: u64,
    /// 已完成（之前文件）的分卷累计
    done_parts: u64,
}

impl EtaTracker {
    /// 新建发送任务时重置
    fn reset(&mut self) {
        self.start = Some(Instant::now());
        self.file = 0;
        self.sent = 0;
        self.done_parts = 0;
    }

    /// 估算剩余秒数；样本不足时返回 None
    fn update(&mut self, p: &SendProgress) -> Option<u64> {
        let file = p.current_file_index.load(Ordering::Relaxed);
        let sent = p.current_file_sent_parts.load(Ordering::Relaxed);

        if file != self.file {
            if self.file > 0 {
                self.done_parts += self.sent;
            }
            self.file = file;
        }
        self.sent = sent;

        let start = self.start?;
        let done = self.done_parts + sent;
        let elapsed = start.elapsed().as_secs_f64();
        if done == 0 || elapsed < 0.5 {
            return None;
        }

        let per_part = elapsed / done as f64;
        let total_parts = p.current_file_total_parts.load(Ordering::Relaxed).max(1);
        let left_parts = total_parts.saturating_sub(sent)
            + total_parts * p.total_files.saturating_sub(file);
        Some((per_part * left_parts as f64).round() as u64)
    }
}

/// 日志窗口的固定 viewport id（固定的 id 保证反复打开只会复用同一个窗口）
fn log_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("mailsender_log_window")
}

pub struct MailApp {
    tab: AppTab,
    /// 全局配置
    pub config: AppConfig,

    /// 日志
    pub logger: Logger,

    /// 发送进度（发送中才有）
    pub progress: Option<Arc<SendProgress>>,

    /// 是否正在发送
    pub sending: bool,

    /* ========== UI 输入状态 ========== */
    /// 收件人
    to: String,

    /// 主题
    subject: String,
    /// 主题是否被用户手动改过（改过则不再被附件自动覆盖）
    subject_dirty: bool,

    /// 正文
    body: String,
    /// 正文是否被用户手动改过
    body_dirty: bool,

    /// 选择的文件
    files: Vec<PathBuf>,

    /* ========== 本次发送的定时（一次性，不持久化） ========== */
    /// 本次发送是否启用定时开始
    sched_enabled: bool,
    /// 选定日期（年/月/日）
    sched_date: (i32, u32, u32),
    /// 选定时间（时/分/秒）
    sched_time: (u32, u32, u32),
    /// 用户是否手动选过时间（没选过时，勾选定时会重置为 now+30min）
    sched_user_set: bool,
    /// 本次发送任务实际使用了定时（用于结束后复位）
    schedule_used_this_run: bool,

    /* ========== 日志窗口（独立 OS 窗口） ========== */
    /// 日志窗口是否应当显示（子窗口收到关闭事件时由回调置 false）
    log_open: Arc<AtomicBool>,
    /// 日志滚动跟随状态（日志窗口渲染、主窗口状态条共读共写）
    log_ui: Arc<Mutex<LogUiState>>,
    /// 本帧处理完后是否请求「打开/前置」日志窗口
    pending_open_log: bool,
    /// 下一次渲染日志窗口时是否发送 Focus（只发一次）
    log_focus_pending: bool,
    /// 日志窗口的初始位置（首次打开时按主窗口位置算一次，之后保持不变）
    log_pos: Option<egui::Pos2>,
    /// 窗口图标（日志窗口复用同一图标）
    app_icon: Option<Arc<egui::IconData>>,

    /// 当前账号编辑缓存
    edit_account: Option<SmtpAccount>,
    /// 当前账号编辑缓存的 ID
    edit_account_id: Option<Uuid>,
    /// 待删除账号的下标（弹确认框）
    pending_delete: Option<usize>,

    /// 账号密码是否明文显示
    show_password: bool,
    /// 7z 压缩密码是否明文显示
    show_sevenz_password: bool,

    /// 发送耗时 / ETA
    eta: EtaTracker,

    /// 品牌 logo 纹理（来自 assets/icon_64.png，首次渲染时懒加载）
    logo_tex: Option<egui::TextureHandle>,

    /// 日志是否以内嵌页显示（窄窗口 / 660px 默认宽度下不再弹第二个 OS 窗口）
    log_inline: bool,
    /// 本帧主窗口是否窄于内嵌日志阈值
    win_narrow: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum AppTab {
    Send,
    Settings,
}

/// 账号行的操作结果
enum AccountRowAction {
    Edit,
    Delete,
}

impl Default for MailApp {
    fn default() -> Self {
        let mut config = AppConfig::load();
        let logger = Logger::new(100);
        // 加载后统一做一次合法性修正，并把修正内容反馈给用户
        for note in config.normalize() {
            logger.warn(format!("配置已修正: {}", note));
        }

        // 旧版全局定时开关迁移：schedule_enabled 不再作为常驻配置。
        // 旧的时间字符串只作为「用户之前选过的时间」预填控件，且必须是未来时间，
        // 不会自动启用定时；其余情况一律用 now+30min 作为默认值。
        let legacy_start = config.parse_schedule_start().ok().flatten();
        if config.schedule_enabled {
            logger.warn(
                "检测到旧版「全局定时开始」配置，已按新语义忽略：定时现在是每次发送的一次性选项（时间已预填到发送页，未启用）"
                    .to_string(),
            );
            config.schedule_enabled = false;
            // 迁移写盘失败不阻塞启动，下一次保存会重试
            let _ = config.save();
        }

        // 定时控件默认值：当前本地时间 + 30 分钟（秒取整为 0）
        let now = chrono::Local::now().naive_local();
        let default_start = schedule::default_start(now);
        let mut sched_date = (default_start.year(), default_start.month(), default_start.day());
        let mut sched_time = (
            default_start.hour(),
            default_start.minute(),
            default_start.second(),
        );
        let mut sched_user_set = false;
        if let Some(t) = legacy_start.filter(|t| *t > now) {
            sched_date = (t.year(), t.month(), t.day());
            sched_time = (t.hour(), t.minute(), t.second());
            sched_user_set = true;
        }

        Self {
            tab: AppTab::Send,
            config,
            logger,
            progress: None,
            sending: false,
            to: String::new(),
            subject: String::new(),
            subject_dirty: false,
            body: String::new(),
            body_dirty: false,
            files: Vec::new(),
            sched_enabled: false,
            sched_date,
            sched_time,
            sched_user_set,
            schedule_used_this_run: false,
            log_open: Arc::new(AtomicBool::new(false)),
            log_ui: Arc::new(Mutex::new(LogUiState::new())),
            pending_open_log: false,
            log_focus_pending: false,
            log_pos: None,
            app_icon: crate::load_app_icon().map(Arc::new),
            edit_account: None,
            edit_account_id: None,
            pending_delete: None,
            show_password: false,
            show_sevenz_password: false,
            eta: EtaTracker::default(),
            logo_tex: None,
            log_inline: false,
            win_narrow: false,
        }
    }
}

impl eframe::App for MailApp {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let win_w = ui.available_width();
        self.win_narrow = win_w < LOG_INLINE_MAX_WIDTH;

        // 后台线程结束后自动复位 sending，否则 UI 会永远停留在「发送中」
        if self.sending {
            let finished = self.progress.as_ref().is_some_and(|p| p.is_finished());
            if finished {
                self.sending = false;
                // 定时是本次发送的一次性属性：任务结束（含停止/取消等待）后复位
                if self.schedule_used_this_run {
                    self.schedule_used_this_run = false;
                    self.sched_enabled = false;
                    // 下次勾选时重新给「当前时间 + 30 分钟」，而不是沿用本次已经过去的时间
                    self.sched_user_set = false;
                    self.logger.info("本次发送已结束，定时开始已复位（下次发送需重新勾选）");
                }
            }
        }

        egui::Panel::top("top")
            .exact_size(TOP_BAR_HEIGHT)
            .resizable(false)
            .show(ui, |ui| {
                let p = theme::palette(ui);
                let full_width = ui.available_width();
                ui.horizontal_centered(|ui| {
                    ui.add_space(2.0);
                    self.brand_logo(ui);
                    ui.add_space(9.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(egui::RichText::new("MailSender").size(14.0).color(p.text));
                        // 窄窗口收起副标题，避免和分段控件、状态文案互相挤压
                        if full_width >= 620.0 {
                            ui.label(
                                egui::RichText::new("邮件发送 · SMTP 轮换")
                                    .size(10.5)
                                    .color(p.text_secondary),
                            );
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(2.0);
                        let status_width = (full_width - 430.0).clamp(0.0, 200.0);
                        if self.sending {
                            if let Some(progress) = &self.progress {
                                if status_width >= 64.0 {
                                    let (text, color) = if progress.is_waiting() {
                                        (
                                            format!(
                                                "等待定时（{}）",
                                                format_left(progress.waiting_left())
                                            ),
                                            p.text_secondary,
                                        )
                                    } else if progress.is_stopped() {
                                        ("正在停止…".to_string(), p.danger)
                                    } else {
                                        ("发送中…".to_string(), p.ok)
                                    };
                                    ui.add_sized(
                                        [status_width, 18.0],
                                        egui::Label::new(
                                            egui::RichText::new(text).size(12.0).color(color),
                                        )
                                        .truncate(),
                                    );
                                }
                            }
                        }
                        ui.add_space(10.0);
                        // 540px 下把分段控件收窄，保证品牌区不被挤压换行
                        let switch_w = if full_width < 600.0 { 124.0 } else { TAB_SWITCH_WIDTH };
                        if tab_switch(ui, &mut self.tab, p, switch_w) {
                            // 切页时退出内嵌日志视图
                            self.log_inline = false;
                        }
                    });
                });
            });

        // 主窗口底部固定一行状态条（替代原来的可拖拽日志面板）：
        // 只占 28px，不再挤压中央区；完整日志在独立日志窗口里看。
        egui::Panel::bottom("status_bar")
            .exact_size(STATUS_BAR_HEIGHT)
            .resizable(false)
            .show(ui, |ui| self.ui_status_bar(ui));

        // 发送页主操作固定在底部，不随滚动区滚走（发送中由进度区接管）
        if self.tab == AppTab::Send && !self.sending && !self.log_inline {
            let p = theme::palette(ui);
            egui::Panel::bottom("send_actions")
                .exact_size(62.0)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(p.surface)
                        .inner_margin(egui::Margin {
                            left: 16,
                            right: 16,
                            top: 8,
                            bottom: 8,
                        }),
                )
                .show(ui, |ui| self.ui_send_actions(ui));
        }

        // 设置页：保存动作统一放在底栏，不随滚动区滚走
        if self.tab == AppTab::Settings && !self.log_inline {
            let p = theme::palette(ui);
            egui::Panel::bottom("settings_actions")
                .exact_size(62.0)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(p.surface)
                        .inner_margin(egui::Margin {
                            left: 16,
                            right: 16,
                            top: 8,
                            bottom: 8,
                        }),
                )
                .show(ui, |ui| self.ui_settings_actions(ui));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin {
                    left: 14,
                    right: 14,
                    top: 14,
                    bottom: 14,
                }),
            )
            .show(ui, |ui| {
                if self.log_inline {
                    self.ui_log_inline(ui);
                } else {
                    match self.tab {
                        AppTab::Send => self.ui_send(ui),
                        AppTab::Settings => self.ui_settings(ui),
                    }
                }
            });

        // 删除账号确认弹窗（挡住其余输入，确认后才真正删除）
        self.show_delete_modal(&ctx);

        // 日志窗口：先处理本帧的「打开/前置」请求，再在需要时重新声明该视口。
        // 必须在 UI 之后做，这样本帧按钮点击产生的请求也能立刻生效。
        if self.pending_open_log {
            self.pending_open_log = false;
            if self.win_narrow {
                // 窄窗口（含默认 660px）不再弹第二个 OS 窗口，改为原位切换
                self.log_inline = true;
            } else {
                self.request_log_window(&ctx);
            }
        }
        if self.log_open.load(Ordering::Relaxed) {
            self.show_log_window(&ctx);
        }

        // 只在需要刷新时请求重绘：发送中刷新进度/倒计时/日志窗口，发送页勾选定时后刷新预览；
        // 空闲时不再固定每 200ms 重绘，避免无意义的 UI 空转。
        if self.log_open.load(Ordering::Relaxed) {
            // 后台线程写日志不会唤醒 UI：只要日志条数比日志窗口上次渲染的多，
            // 就主动唤醒子窗口一次（发送中每 200ms 一次，空闲时只在真有新日志时触发）
            let seen = self.log_ui.lock().seen;
            if self.logger.len() > seen {
                ctx.request_repaint_of(log_viewport_id());
            }
        } else if self.log_inline {
            // 内嵌日志在发送中持续刷新
            let seen = self.log_ui.lock().seen;
            if self.logger.len() > seen {
                ctx.request_repaint();
            }
        }

        if self.sending {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        } else if self.tab == AppTab::Send && self.sched_enabled {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
}

impl MailApp {
    /* =================== 发送页 =================== */
    fn ui_send(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ---------- ① 附件 ----------
            section(ui, "① 选择附件", |ui| {
                // 任意时刻拖入文件都能追加（空态有拖放提示区，非空态同样生效）
                let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
                    i.raw
                        .dropped_files
                        .iter()
                        .map(|f| f.path().to_path_buf())
                        // 只接受真实存在的文件（忽略拖入的文本/内存对象）
                        .filter(|p| p.is_file())
                        .collect()
                });
                if !dropped.is_empty() {
                    let n = dropped.len();
                    self.files.extend(dropped);
                    self.refresh_autofill();
                    self.logger.info(format!("已通过拖放添加 {} 个文件", n));
                }

                let p = theme::palette(ui);
                ui.horizontal(|ui| {
                    if button(ui, BtnKind::Secondary, Some(Icon::Plus), "添加文件").clicked() {
                        if let Some(mut files) = rfd::FileDialog::new().pick_files() {
                            self.files.append(&mut files);
                            self.refresh_autofill();
                        }
                    }
                    if !self.files.is_empty() {
                        if button(ui, BtnKind::Ghost, Some(Icon::Trash), "清空").clicked() {
                            self.files.clear();
                            self.refresh_autofill();
                        }
                        if button(ui, BtnKind::Ghost, Some(Icon::Refresh), "重新生成主题/正文")
                            .on_hover_text("覆盖当前主题与正文（包括手动修改过的内容）")
                            .clicked()
                        {
                            self.subject_dirty = false;
                            self.body_dirty = false;
                            self.refresh_autofill();
                            self.logger.info("已根据附件重新生成主题与正文");
                        }
                    }
                });

                ui.add_space(theme::SPACE_SM);

                if self.files.is_empty() {
                    if drop_zone(ui) {
                        if let Some(mut files) = rfd::FileDialog::new().pick_files() {
                            self.files.append(&mut files);
                            self.refresh_autofill();
                        }
                    }
                } else {
                    let mut remove: Option<usize> = None;
                    egui::ScrollArea::vertical()
                        .id_salt("attachment_list")
                        .max_height(190.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (i, f) in self.files.iter().enumerate() {
                                if attachment_row(ui, f) {
                                    remove = Some(i);
                                }
                            }
                        });
                    if let Some(i) = remove {
                        self.files.remove(i);
                        self.refresh_autofill();
                    }

                    ui.add_space(theme::SPACE_SM);
                    if let Some(sum) = autofill::summarize_attachments(&self.files) {
                        ui.label(
                            egui::RichText::new(format!(
                                "共 {} 个 / 总大小 {}",
                                sum.count,
                                autofill::format_size(sum.total_bytes)
                            ))
                            .size(11.5)
                            .color(p.text_secondary),
                        );
                    }
                }
            });

            // ---------- ② 收件人与内容 ----------
            section(ui, "② 收件人与内容", |ui| {
                egui::Grid::new("send_fields")
                    .num_columns(2)
                    .min_col_width(56.0)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                    field_label(ui, "收件人");
                    ui.horizontal(|ui| {
                        // 输入框宽度自适应：给右侧「历史」下拉预留约 130px
                        let input_width = (ui.available_width() - 130.0).clamp(140.0, 420.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.to)
                                .hint_text("多个收件人用英文逗号分隔")
                                .desired_width(input_width),
                        );
                        egui::ComboBox::from_id_salt("recipient_history")
                            .width(120.0)
                            .selected_text("历史")
                            .show_ui(ui, |ui| {
                                for email in &self.config.history.recipients {
                                    if chip_button(ui, email, false).clicked() {
                                        self.to = email.clone();
                                    }
                                }
                            });
                    });
                    ui.end_row();

                    field_label(ui, "主题");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut self.subject)
                                .desired_width(f32::INFINITY),
                        )
                        .changed()
                    {
                        self.subject_dirty = true;
                    }
                    ui.end_row();

                    field_label(ui, "正文");
                    if ui
                        .add(
                            egui::TextEdit::multiline(&mut self.body)
                                .desired_width(f32::INFINITY)
                                .desired_rows(5),
                        )
                        .changed()
                    {
                        self.body_dirty = true;
                    }
                    ui.end_row();
                });
            });

            // ---------- ③ 发送选项 ----------
            section(ui, "③ 发送选项", |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("每封邮件随机间隔");
                    let mut min = self.config.interval_min_sec.unwrap_or(self.config.interval_sec);
                    let mut max = self
                        .config
                        .interval_max_sec
                        .unwrap_or(min.max(self.config.interval_sec));
                    let r1 = ui
                        .add_sized(
                            [96.0, 28.0],
                            egui::DragValue::new(&mut min)
                                .range(0..=86_400u64)
                                .prefix("最小 ")
                                .suffix(" 秒"),
                        )
                        .changed();
                    let r2 = ui
                        .add_sized(
                            [96.0, 28.0],
                            egui::DragValue::new(&mut max)
                                .range(0..=86_400u64)
                                .prefix("最大 ")
                                .suffix(" 秒"),
                        )
                        .changed();
                    if r1 || r2 {
                        self.config.interval_min_sec = Some(min);
                        self.config.interval_max_sec = Some(max);
                    }
                    ui.weak(format!(
                        "实际区间 {} ~ {} 秒",
                        self.config.interval_range().0,
                        self.config.interval_range().1
                    ));
                });
                if self
                    .config
                    .interval_min_sec
                    .zip(self.config.interval_max_sec)
                    .is_some_and(|(min, max)| min > max)
                {
                    ui.colored_label(
                        theme::palette(ui).danger,
                        "最小值大于最大值，发送时会自动交换",
                    );
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                // ===== 定时开始（仅本次发送）：开关 + 摘要 chip 一行，展开后一行控件 =====
                let p = theme::palette(ui);
                let mut toggled = false;
                ui.horizontal(|ui| {
                    toggled = toggle_switch(ui, &mut self.sched_enabled).changed();
                    ui.label(egui::RichText::new("定时开始").size(12.5).color(p.text));

                    let (text, fg, bg) = if !self.sched_enabled {
                        ("未启用".to_string(), p.text_secondary, p.surface_alt)
                    } else {
                        match self.schedule_target() {
                            Ok(t) => {
                                let now = chrono::Local::now().naive_local();
                                if t > now {
                                    let left = (t - now).num_seconds() as u64;
                                    (
                                        format!(
                                            "{} · 剩余 {}",
                                            t.format("%m-%d %H:%M"),
                                            format_left(left)
                                        ),
                                        p.accent,
                                        p.accent_soft,
                                    )
                                } else {
                                    (
                                        format!("{} · 时间已过，将立即发送", t.format("%m-%d %H:%M")),
                                        p.warn,
                                        tint(p.warn, 30),
                                    )
                                }
                            }
                            Err(_) => ("时间无效".to_string(), p.danger, tint(p.danger, 30)),
                        }
                    };
                    chip(ui, &text, fg, bg);
                });

                // 勾选时：用户还没手动选过时间，就重置为「当前本地时间 + 30 分钟」
                if toggled && self.sched_enabled && !self.sched_user_set {
                    self.reset_schedule_default();
                }

                // 展开后只保留：日期 / 时间两个 DragValue + −10m / −5m / +10m 预设
                let state_id = ui.scope_id().with("schedule_body");
                let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    state_id,
                    self.sched_enabled,
                );
                state.set_open(self.sched_enabled);

                let mut edited = false;
                state.show_body_unindented(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("开始").size(12.0).color(p.text_secondary));
                        edited |= date_drag(ui, &mut self.sched_date);
                        edited |= time_drag(ui, &mut self.sched_time);
                        ui.add_space(theme::SPACE_XS);
                        if button(ui, BtnKind::Ghost, None, "-10 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(-10));
                        }
                        if button(ui, BtnKind::Ghost, None, "-5 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(-5));
                        }
                        if button(ui, BtnKind::Ghost, None, "+10 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(10));
                        }
                    });
                });
                if edited {
                    // 手动改过日期/时间后，后续勾选/取消不再覆盖用户选择
                    self.sched_user_set = true;
                }
            });

            // ---------- ④ 进度（空闲时主操作在底部固定栏）----------
            if self.sending {
                if let Some(progress) = &self.progress {
                    ui.add_space(6.0);
                    let progress = progress.clone();
                    self.ui_progress(ui, progress);
                }
            }
        });
    }

    /// 发送页底部固定操作栏：左侧就绪摘要，右侧主操作
    fn ui_send_actions(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let ready = !self.files.is_empty()
            && !self.to.trim().is_empty()
            && !self.config.accounts.is_empty();
        let right_width = 150.0;
        let left_width = (ui.available_width() - right_width).max(80.0);

        ui.horizontal_centered(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(left_width, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let (summary, dot) = if ready {
                        (
                            format!(
                                "已就绪 · {} 个附件 · {} 个账号",
                                self.files.len(),
                                self.config.accounts.len()
                            ),
                            p.ok,
                        )
                    } else {
                        (
                            "需要先选择附件、填写收件人，并在「设置」页配置 SMTP 账号".to_string(),
                            p.text_secondary,
                        )
                    };
                    ui.label(egui::RichText::new("●").color(dot));
                    ui.add_space(theme::SPACE_XS);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(summary).size(12.0).color(p.text_secondary),
                        )
                        .truncate(),
                    );
                },
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button_cta(ui, ready, "开始发送").clicked() {
                    self.start_send();
                }
            });
        });
    }

    /// 附件变化后按脏标记刷新主题/正文
    fn refresh_autofill(&mut self) {
        if let Some(v) = autofill::next_field_value(
            autofill::suggest_subject(&self.files).as_deref(),
            self.subject_dirty,
        ) {
            self.subject = v;
        }
        if let Some(v) = autofill::next_field_value(
            autofill::suggest_body(
                &self.files,
                self.config.split_mb,
                !self.config.sevenz_password.is_empty(),
            )
            .as_deref(),
            self.body_dirty,
        ) {
            self.body = v;
        }
    }

    /// 把控件时间整体平移（快捷按钮用）；用 chrono 做真实日期运算，跨天/跨月/跨年自动进位
    fn shift_schedule(&mut self, delta: Duration) {
        let base = self
            .schedule_target()
            .unwrap_or_else(|_| chrono::Local::now().naive_local());
        match schedule::shift(base, delta) {
            Some(t) => {
                self.set_schedule_time(t);
                self.sched_user_set = true;
            }
            None => self.logger.warn("定时时间超出可表示范围，未修改"),
        }
    }

    /// 勾选定时且用户尚未选过时间时，把控件重置为「当前本地时间 + 30 分钟」
    fn reset_schedule_default(&mut self) {
        let t = schedule::default_start(chrono::Local::now().naive_local());
        self.set_schedule_time(t);
        self.sched_user_set = false;
    }

    /// 把日期时间写回控件的年/月/日/时/分/秒
    fn set_schedule_time(&mut self, t: NaiveDateTime) {
        self.sched_date = (t.year(), t.month(), t.day());
        self.sched_time = (t.hour(), t.minute(), t.second());
    }

    /// 由控件状态解析出目标时间；非法日期（如 2 月 30 日）返回错误
    fn schedule_target(&self) -> Result<NaiveDateTime, String> {
        let (y, m, d) = self.sched_date;
        let (hh, mm, ss) = self.sched_time;
        let date = NaiveDate::from_ymd_opt(y, m, d)
            .ok_or_else(|| format!("{}-{:02}-{:02} 不是有效日期", y, m, d))?;
        let time = NaiveTime::from_hms_opt(hh, mm, ss)
            .ok_or_else(|| format!("{:02}:{:02}:{:02} 不是有效时间", hh, mm, ss))?;
        Ok(NaiveDateTime::new(date, time))
    }

    /// 三阶段进度：单主进度条 + 阶段文案（压缩中 / 发送中 / 等待中 / 已暂停）+ 卷数 + ETA
    fn ui_progress(&mut self, ui: &mut egui::Ui, progress: Arc<SendProgress>) {
        let p = theme::palette(ui);

        let waiting = progress.is_waiting();
        let paused = progress.is_paused();
        let stopped = progress.is_stopped();

        let cur_file = progress.current_file_index.load(Ordering::Relaxed);
        let total_files = progress.total_files.max(1);
        let total_parts = progress.current_file_total_parts.load(Ordering::Relaxed);
        let sent_parts = progress.current_file_sent_parts.load(Ordering::Relaxed);

        // 压缩阶段：文件已开始、但分卷总数尚未确定（split_if_needed 还在跑）
        let compressing = !waiting && !paused && !stopped && cur_file > 0 && total_parts == 0;

        let (stage, stage_color) = if stopped {
            ("正在停止…", p.danger)
        } else if paused {
            ("已暂停", p.warn)
        } else if waiting {
            ("等待中", p.text_secondary)
        } else if compressing {
            ("压缩中", p.text_secondary)
        } else {
            ("发送中", p.ok)
        };

        let eta = if waiting || stopped { None } else { self.eta.update(&progress) };

        ui.horizontal(|ui| {
            if compressing || stopped || waiting {
                ui.add(egui::Spinner::new().size(14.0).color(stage_color));
            } else {
                let dot = egui::RichText::new("●").size(10.0).color(stage_color);
                ui.label(dot);
            }
            ui.label(egui::RichText::new(stage).size(12.5).color(stage_color));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(eta) = eta {
                    ui.label(
                        egui::RichText::new(format!("预计剩余 {}", format_left(eta)))
                            .size(11.0)
                            .color(p.text_secondary),
                    );
                }
                ui.label(
                    egui::RichText::new(format!("卷 {}/{}", sent_parts, total_parts.max(1)))
                        .size(11.0)
                        .color(p.text_secondary),
                );
                ui.label(
                    egui::RichText::new(format!("文件 {}/{}", cur_file.min(total_files), total_files))
                        .size(11.0)
                        .color(p.text_secondary),
                );
            });
        });

        // 单主进度条：压缩阶段用 Spinner（不显示 0%），其余阶段平滑过渡
        let part_ratio = if total_parts == 0 {
            0.0
        } else {
            sent_parts as f32 / total_parts as f32
        };
        let raw = ((cur_file.saturating_sub(1)) as f32 + part_ratio) / total_files as f32;

        if !compressing {
            let shown = ui
                .ctx()
                .animate_value_with_time(egui::Id::unique("send_main_progress"), raw, 0.25);
            let percent = if waiting {
                String::new()
            } else {
                format!("{:.0}%", shown * 100.0)
            };
            ui.add(
                egui::ProgressBar::new(shown)
                    .desired_height(12.0)
                    .corner_radius(egui::CornerRadius::same(6))
                    .fill(if stopped { p.danger } else { p.accent })
                    .text(egui::RichText::new(percent).size(11.0).color(p.text)),
            );
        } else {
            // 压缩中：不显示会误导的 0%，只留一条轨道
            ui.add(
                egui::ProgressBar::new(0.0)
                    .desired_height(12.0)
                    .corner_radius(egui::CornerRadius::same(6))
                    .fill(p.accent),
            );
        }

        ui.add_space(theme::SPACE_XS);
        ui.horizontal(|ui| {
            if waiting {
                if button(ui, BtnKind::Ghost, Some(Icon::Stop), "取消等待").clicked() {
                    progress.stopped.store(true, Ordering::Relaxed);
                }
                ui.label(
                    egui::RichText::new(format!(
                        "定时开始后自动发送，剩余 {}",
                        format_left(progress.waiting_left())
                    ))
                    .size(11.0)
                    .color(p.text_secondary),
                );
            } else {
                if paused {
                    if button(ui, BtnKind::Secondary, Some(Icon::Play), "继续").clicked() {
                        progress.paused.store(false, Ordering::Relaxed);
                    }
                } else if button(ui, BtnKind::Secondary, Some(Icon::Pause), "暂停").clicked() {
                    progress.paused.store(true, Ordering::Relaxed);
                }
                if button(ui, BtnKind::Ghost, Some(Icon::Stop), "停止").clicked() {
                    // 只置停止标记，等后台线程真正结束后由 ui() 复位 sending
                    progress.stopped.store(true, Ordering::Relaxed);
                }
            }
        });
    }

    fn start_send(&mut self) {
        if self.files.is_empty() {
            self.logger
                .error("未选择文件，无法发送（请先在「选择附件」中添加文件）");
            return;
        }
        if self.config.accounts.is_empty() {
            self.logger
                .error("没有可用的 SMTP 账号，无法发送（请到「设置」页添加账号）");
            return;
        }
        if self.to.trim().is_empty() {
            self.logger.error("收件人为空，无法发送（请填写收件人）");
            return;
        }

        // 本次发送的定时（一次性）：非法时间直接阻止，不静默
        let schedule_at = if self.sched_enabled {
            match self.schedule_target() {
                Ok(t) => {
                    // 用直接比较而非 num_seconds()，避免「还差不到 1 秒」被当成已过
                    if t > chrono::Local::now().naive_local() {
                        Some(t)
                    } else {
                        self.logger.warn(format!(
                            "定时开始时间 {} 已过，将立即发送",
                            t.format("%Y-%m-%d %H:%M:%S")
                        ));
                        None
                    }
                }
                Err(e) => {
                    self.logger.error(format!("定时开始配置无效: {e}"));
                    return;
                }
            }
        } else {
            None
        };

        // 发送前统一修正配置，避免 min > max 等异常区间
        for note in self.config.normalize() {
            self.logger.warn(format!("配置已修正: {}", note));
        }

        let (min_sec, max_sec) = self.config.interval_range();
        self.logger.info(format!("本次发送随机间隔区间: {} ~ {} 秒", min_sec, max_sec));

        let progress = Arc::new(SendProgress::new(self.files.len() as u64));
        let logger = self.logger.clone();
        let config = self.config.clone();
        let files = self.files.clone();
        let to = self.to.clone();
        let subject = self.subject.clone();
        let body = self.body.clone();

        self.progress = Some(progress.clone());
        self.sending = true;
        self.eta.reset();
        self.schedule_used_this_run = schedule_at.is_some();
        // 开始发送时自动打开日志：窄窗口原位显示，宽窗口复用独立日志窗口
        if self.win_narrow {
            self.log_inline = true;
        } else {
            self.pending_open_log = true;
        }
        self.config.history.record(&to);
        thread::spawn(move || {
            start_send_task(&files, &to, &subject, &body, &config, schedule_at, progress, logger);
        });
    }

    /* =================== 设置页 =================== */

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ===== 账号列表（每行一张卡片）=====
            let mut start_edit: Option<(Option<usize>, SmtpAccount)> = None;
            let mut ask_delete: Option<usize> = None;

            section(ui, "SMTP 账号", |ui| {
                let p = theme::palette(ui);
                if self.config.accounts.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "尚未配置账号 —— 请点击下方「添加账号」填写 SMTP 信息后再发送",
                        )
                        .size(12.0)
                        .color(p.text_secondary),
                    );
                    ui.add_space(theme::SPACE_SM);
                }

                for (i, acc) in self.config.accounts.iter().enumerate() {
                    match account_row(ui, acc) {
                        Some(AccountRowAction::Edit) => {
                            start_edit = Some((Some(i), acc.clone()));
                        }
                        Some(AccountRowAction::Delete) => ask_delete = Some(i),
                        None => {}
                    }
                    ui.add_space(theme::SPACE_XS);
                }

                if button(ui, BtnKind::Secondary, Some(Icon::Plus), "添加账号").clicked() {
                    start_edit = Some((
                        None,
                        SmtpAccount {
                            id: Uuid::new_v4(),
                            name: String::new(),
                            smtp: String::new(),
                            port: 465,
                            username: String::new(),
                            password: String::new(),
                            ssl: true,
                        },
                    ));
                }
            });

            if let Some(i) = ask_delete {
                self.pending_delete = Some(i);
            }
            // ===== 进入编辑态（UI 后处理）=====
            if let Some((idx, acc)) = start_edit {
                // 必须记录被编辑账号的 ID，否则保存时会被当成「新增」而重复插入账号
                self.edit_account_id = if idx.is_some() { Some(acc.id) } else { None };
                self.edit_account = Some(acc);
            }

            // ===== 编辑面板（保存/取消统一在设置页底栏）=====
            if self.edit_account.is_some() {
                let mut acc = self.edit_account.take().expect("上面已判断存在");
                let mut show_pw = self.show_password;
                // 入场淡入（展开/收起统一使用 animate_bool 的时长曲线）
                let open_t = ui
                    .ctx()
                    .animate_bool_with_time(ui.scope_id().with("account_edit_open"), true, 0.2);

                ui.scope(|ui| {
                    ui.set_opacity(open_t);
                    section(ui, "账号配置", |ui| {
                        let p = theme::palette(ui);
                    egui::Grid::new("account_edit")
                        .num_columns(2)
                        .min_col_width(72.0)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            field_label(ui, "名称");
                            ui.add(
                                egui::TextEdit::singleline(&mut acc.name)
                                    .hint_text("例如：主账号")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            // 服务器与端口同一行，端口固定宽度
                            field_label(ui, "服务器");
                            ui.horizontal(|ui| {
                                let port_w = 78.0;
                                let w = (ui.available_width() - port_w - 8.0).max(120.0);
                                ui.add(
                                    egui::TextEdit::singleline(&mut acc.smtp)
                                        .hint_text("smtp.example.com")
                                        .desired_width(w),
                                );
                                ui.add_sized(
                                    [port_w, 28.0],
                                    egui::DragValue::new(&mut acc.port).range(1..=65535),
                                );
                            });
                            ui.end_row();

                            field_label(ui, "登录名");
                            ui.add(
                                egui::TextEdit::singleline(&mut acc.username)
                                    .hint_text("通常是邮箱地址")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.end_row();

                            field_label(ui, "密码");
                            ui.horizontal(|ui| {
                                let w = (ui.available_width() - 78.0).max(120.0);
                                ui.add(
                                    egui::TextEdit::singleline(&mut acc.password)
                                        .hint_text("授权码 / 密码")
                                        .password(!show_pw)
                                        .desired_width(w),
                                );
                                if button(
                                    ui,
                                    BtnKind::Ghost,
                                    None,
                                    if show_pw { "隐藏" } else { "显示" },
                                )
                                .clicked()
                                {
                                    show_pw = !show_pw;
                                }
                            });
                            ui.end_row();

                            field_label(ui, "SSL");
                            ui.checkbox(&mut acc.ssl, "465 端口一般勾选；587 一般不勾");
                            ui.end_row();
                        });

                        ui.add_space(theme::SPACE_XS);
                        ui.label(
                            egui::RichText::new("修改完成后，请在页面底栏点击「保存账号」写入配置")
                                .size(11.0)
                                .color(p.text_secondary),
                        );
                    });
                });

                self.show_password = show_pw;
                self.edit_account = Some(acc);
            }

            // ===== 7z 分卷与加密 =====
            section(ui, "7z 分卷与加密", |ui| {
                let p = theme::palette(ui);
                ui.horizontal(|ui| {
                    field_label(ui, "分卷大小");
                    ui.add_sized(
                        [96.0, 28.0],
                        egui::DragValue::new(&mut self.config.split_mb)
                            .range(1..=10_000u64)
                            .suffix(" MB"),
                    );
                    ui.label(
                        egui::RichText::new("超出后自动分卷压缩")
                            .size(11.0)
                            .color(p.text_secondary),
                    );
                });

                ui.add_space(theme::SPACE_SM);
                ui.horizontal(|ui| {
                    field_label(ui, "压缩密码");
                    let show = self.show_sevenz_password;
                    let w = (ui.available_width() - 78.0).max(140.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.sevenz_password)
                            .hint_text("留空则不加密")
                            .password(!show)
                            .desired_width(w),
                    );
                    if button(ui, BtnKind::Ghost, None, if show { "隐藏" } else { "显示" }).clicked()
                    {
                        self.show_sevenz_password = !self.show_sevenz_password;
                    }
                });

                if self.config.sevenz_password.is_empty() {
                    ui.add_space(theme::SPACE_XS);
                    ui.label(
                        egui::RichText::new(
                            "密码为空 —— 附件将以不加密方式发送；如需保护内容，请填写压缩密码",
                        )
                        .size(11.5)
                        .color(p.warn),
                    );
                }
            });
        });
    }

    /// 设置页底栏：编辑中显示「保存账号 / 取消」，否则显示「保存配置」
    fn ui_settings_actions(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        let editing = self.edit_account.is_some();
        let right_width = if editing { 236.0 } else { 150.0 };
        let left_width = (ui.available_width() - right_width).max(80.0);

        ui.horizontal_centered(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(left_width, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(egui::RichText::new("●").color(if editing { p.warn } else { p.ok }));
                    ui.add_space(theme::SPACE_XS);
                    let summary = if editing {
                        "账号信息编辑中 —— 点击「保存账号」写入配置"
                    } else {
                        "配置修改后请点击「保存配置」写入配置文件"
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(summary).size(12.0).color(p.text_secondary),
                        )
                        .truncate(),
                    );
                },
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if editing {
                    if button(ui, BtnKind::Primary, Some(Icon::Save), "保存账号").clicked() {
                        self.save_edit_account();
                    }
                    if button(ui, BtnKind::Ghost, None, "取消").clicked() {
                        self.edit_account = None;
                        self.edit_account_id = None;
                    }
                } else if button(ui, BtnKind::Primary, Some(Icon::Save), "保存配置").clicked() {
                    for note in self.config.normalize() {
                        self.logger.warn(format!("配置已修正: {}", note));
                    }
                    self.save_config();
                }
            });
        });
    }

    /// 把编辑缓存写入账号列表并落盘
    fn save_edit_account(&mut self) {
        if let Some(acc) = self.edit_account.take() {
            match self.edit_account_id {
                Some(id) => {
                    if let Some(target) = self.config.accounts.iter_mut().find(|a| a.id == id) {
                        *target = acc;
                    } else {
                        self.config.accounts.push(acc);
                    }
                }
                None => {
                    self.config.accounts.push(acc);
                }
            }
            self.save_config();
        }
        self.edit_account_id = None;
    }

    /// 删除账号确认弹窗（Modal 会挡住其余输入）
    fn show_delete_modal(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_delete else {
            return;
        };
        let Some(acc) = self.config.accounts.get(idx) else {
            self.pending_delete = None;
            return;
        };
        let label = if acc.name.trim().is_empty() {
            acc.smtp.clone()
        } else {
            format!("{}（{}）", acc.name, acc.smtp)
        };

        let mut confirm = false;
        let mut cancel = false;
        let resp = egui::Modal::new(egui::Id::unique("delete_account_modal")).show(ctx, |ui| {
            let p = theme::palette(ui);
            ui.set_max_width(360.0);
            ui.label(egui::RichText::new("删除账号").size(15.0).color(p.text));
            ui.add_space(theme::SPACE_SM);
            ui.label(
                egui::RichText::new(format!(
                    "确认删除「{}」？删除后无法恢复，只能重新填写 SMTP 信息。",
                    label
                ))
                .size(12.5)
                .color(p.text_secondary),
            );
            ui.add_space(theme::SPACE_MD);
            ui.horizontal(|ui| {
                if button(ui, BtnKind::Ghost, None, "取消").clicked() {
                    cancel = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button_tinted(
                        ui,
                        BtnKind::Primary,
                        None,
                        "删除",
                        Some(p.danger),
                        Some(egui::Color32::WHITE),
                    )
                    .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        });

        if resp.backdrop_response.clicked() {
            cancel = true;
        }

        if cancel {
            self.pending_delete = None;
        }
        if confirm {
            if let Some(i) = self.pending_delete.take() {
                if i < self.config.accounts.len() {
                    let removed = self.config.accounts.remove(i);
                    if Some(removed.id) == self.edit_account_id {
                        self.edit_account = None;
                        self.edit_account_id = None;
                    }
                    let name = if removed.name.trim().is_empty() {
                        removed.smtp.clone()
                    } else {
                        removed.name.clone()
                    };
                    self.save_config();
                    self.logger.info(format!("已删除账号「{}」", name));
                }
            }
        }
    }

    /// 保存配置并把失败原因写进日志（不再静默吞掉写盘错误）
    fn save_config(&mut self) {
        match self.config.save() {
            Ok(()) => self.logger.info("配置已保存"),
            Err(e) => self.logger.error(format!("保存配置失败: {e}")),
        }
    }

    /* =================== 底部状态条 =================== */

    /// 主窗口底部一行状态条：最新日志 + 总条数 + 跟随状态 + 打开日志按钮
    fn ui_status_bar(&mut self, ui: &mut egui::Ui) {
        let entries = self.logger.entries();
        let total = entries.len();
        let follow = self.log_ui.lock().follow;

        // 右侧固定块宽度（打开日志 + 共 N 条 + 跟随状态 + 进度文字）
        let right_width = if self.sending { 300.0 } else { 216.0 };
        let left_width = (ui.available_width() - right_width).max(80.0);

        ui.horizontal_centered(|ui| {
            // 左侧：最新一条日志（宽度不足时截断）
            ui.allocate_ui_with_layout(
                egui::vec2(left_width, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| match entries.last() {
                    Some(log) => {
                        ui.label(egui::RichText::new("●").color(level_color(ui, log.level)));
                        ui.add(
                            egui::Label::new(format!("[{}] {}", log.time, log.message))
                                .truncate(),
                        );
                    }
                    None => {
                        ui.weak("暂无日志 · 开始发送后这里会显示运行记录");
                    }
                },
            );

            // 右侧：进度状态 / 跟随暂停 / 条数 / 打开日志（从右到左排）
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let p = theme::palette(ui);
                let log_btn = if self.log_inline {
                    button(ui, BtnKind::Ghost, Some(Icon::Close), "关闭日志")
                } else {
                    button(ui, BtnKind::Ghost, Some(Icon::Log), "打开日志")
                };
                if log_btn
                    .on_hover_text(if self.win_narrow {
                        "在当前窗口内查看运行日志"
                    } else {
                        "打开独立的日志窗口（已打开则前置）"
                    })
                    .clicked()
                {
                    if self.log_inline {
                        // 正在内嵌显示时，这个按钮就是「关闭日志」
                        self.log_inline = false;
                    } else if self.win_narrow {
                        // 窄窗口原位切换，不再弹第二个 OS 窗口
                        self.log_inline = true;
                    } else {
                        self.pending_open_log = true;
                    }
                }
                ui.label(
                    egui::RichText::new(format!("共 {} 条", total))
                        .size(11.0)
                        .color(p.text_secondary),
                );
                if !follow {
                    ui.label(
                        egui::RichText::new("已暂停跟随")
                            .size(11.0)
                            .color(p.text_secondary),
                    )
                    .on_hover_text("日志已暂停自动跟随（滚回底部或点击「回到底部」恢复）");
                }
                if self.sending {
                    if let Some(progress) = &self.progress {
                        ui.label(
                            egui::RichText::new(progress_summary(progress))
                                .size(11.0)
                                .color(p.text_secondary),
                        );
                    }
                }
            });
        });
    }

    /* =================== 日志窗口 =================== */

    /// 请求打开/前置日志窗口；首次打开时按主窗口位置计算一个不遮挡它的初始位置
    fn request_log_window(&mut self, ctx: &egui::Context) {
        if !self.log_open.swap(true, Ordering::Relaxed) {
            let root = ctx.input(|i| i.viewport().outer_rect);
            let monitor = ctx.input(|i| i.viewport().monitor_size);
            self.log_pos = root.map(|r| {
                let mut pos = egui::pos2(r.max.x + 16.0, r.min.y + 28.0);
                if let Some(m) = monitor {
                    // 贴近屏幕右缘时向左回退，避免窗口跑到屏幕外
                    pos.x = pos.x.min((m.x - LOG_WINDOW_SIZE[0] - 12.0).max(12.0));
                    pos.y = pos.y.clamp(12.0, (m.y - LOG_WINDOW_SIZE[1] - 12.0).max(12.0));
                }
                pos
            });
        }
        self.log_focus_pending = true;
        ctx.request_repaint();
    }

    /// 内嵌日志页：宽度不足 760px 时替代第二个 OS 窗口
    fn ui_log_inline(&mut self, ui: &mut egui::Ui) {
        let entries = self.logger.entries();
        let total = entries.len();
        let (close, follow) = {
            let mut st = self.log_ui.lock();
            let close = log_header(ui, &mut st, total, true);
            ui.separator();
            draw_log_scroll(ui, &entries, &mut st);
            (close, st.follow)
        };
        if close {
            self.log_inline = false;
            ui.ctx().request_repaint();
        }
        if !follow {
            ui.ctx().request_repaint();
        }
    }

    /// 每帧声明日志视口（固定 ViewportId → 永远复用同一个 OS 窗口，不会重复开窗）
    fn show_log_window(&mut self, ctx: &egui::Context) {
        let mut builder = egui::ViewportBuilder::default()
            .with_title("运行日志 — MailSender")
            .with_inner_size(LOG_WINDOW_SIZE)
            .with_min_inner_size(LOG_WINDOW_MIN_SIZE)
            .with_resizable(true);
        if let Some(icon) = &self.app_icon {
            builder = builder.with_icon(icon.clone());
        }
        if let Some(pos) = self.log_pos {
            builder = builder.with_position(pos);
        }

        let open = self.log_open.clone();
        let logger = self.logger.clone();
        let shared = self.log_ui.clone();

        ctx.show_viewport_deferred(log_viewport_id(), builder, move |ui, _class| {
            // 用户点了子窗口的关闭按钮：只把「应显示」置 false，
            // 主窗口下一帧不再声明该视口，eframe 自会销毁这个 OS 窗口；
            // 发送线程/进度/状态条都不经过视口，因此不受影响。
            if ui.input(|i| i.viewport().close_requested()) {
                open.store(false, Ordering::Relaxed);
                ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
            }

            egui::CentralPanel::default().show(ui, |ui| {
                let entries = logger.entries();
                let mut st = shared.lock();
                let follow_before = st.follow;

                log_header(ui, &mut st, entries.len(), false);
                draw_log_scroll(ui, &entries, &mut st);

                // 跟随状态变化时唤醒主窗口，让状态条的「已暂停跟随」提示同步
                if st.follow != follow_before {
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                }
            });
        });

        if self.log_focus_pending {
            self.log_focus_pending = false;
            ctx.send_viewport_cmd_to(log_viewport_id(), egui::ViewportCommand::Focus);
        }
    }
}

impl MailApp {
    /// 顶栏品牌 logo：优先用 assets 里的图标（懒加载纹理），失败时退回信封线稿
    fn brand_logo(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui);
        const LOGO_PNG: &[u8] = include_bytes!("../assets/icon_128.png");

        if self.logo_tex.is_none() {
            if let Ok(icon) = eframe::icon_data::from_png_bytes(LOGO_PNG) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                );
                self.logo_tex = Some(ui.ctx().load_texture(
                    "mailsender_logo",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }

        let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
        let painter = ui.painter();
        let radius = egui::CornerRadius::same(theme::CONTROL_RADIUS);
        painter.rect_filled(rect, radius, p.surface_alt);

        match &self.logo_tex {
            Some(tex) => {
                painter.image(
                    tex.id(),
                    rect.shrink(1.0),
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                let inset = rect.shrink(7.0);
                let stroke = egui::Stroke::new(1.2, p.text_secondary);
                painter.rect_stroke(
                    inset,
                    egui::CornerRadius::same(2),
                    stroke,
                    egui::StrokeKind::Inside,
                );
                let mid = egui::pos2(inset.center().x, inset.top() + inset.height() * 0.42);
                painter.line_segment([inset.left_top(), mid], stroke);
                painter.line_segment([inset.right_top(), mid], stroke);
            }
        }

        painter.rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
    }
}

/// 日志窗口标题行（等级筛选 + 跟随状态 + 回到顶部按钮），返回「是否请求关闭内嵌视图」
fn log_header(ui: &mut egui::Ui, st: &mut LogUiState, total: usize, inline: bool) -> bool {
    let p = theme::palette(ui);
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("运行日志").size(13.5).color(p.text));
        ui.label(
            egui::RichText::new(format!("共 {} 条", total))
                .size(11.0)
                .color(p.text_secondary),
        );
        ui.add_space(theme::SPACE_MD);

        // 等级筛选（顶部一行，三个 chip）
        for f in [LogFilter::All, LogFilter::Warn, LogFilter::Error] {
            if chip_button(ui, f.label(), st.filter == f).clicked() {
                st.filter = f;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if inline && button(ui, BtnKind::Ghost, Some(Icon::Close), "返回").clicked() {
                close = true;
            }
            if st.follow {
                ui.label(
                    egui::RichText::new("自动跟随中")
                        .size(11.0)
                        .color(p.text_secondary),
                );
            } else if button(ui, BtnKind::Ghost, Some(Icon::ArrowDown), "回到底部").clicked() {
                st.follow = true;
                st.jump = true;
            }
        });
    });
    close
}

/// 日志滚动区（含「不操作时自动跟随」逻辑），日志窗口与状态条共用同一份 `LogUiState`
fn draw_log_scroll(ui: &mut egui::Ui, entries: &[LogItem], st: &mut LogUiState) {
    let has_new = entries.len() > st.seen;

    // 鼠标按住且位于日志滚动区内（拖动滚动条 / 选中文本）时，本帧不抢滚动
    let pointer_busy = ui.ctx().input(|i| i.pointer.any_down())
        && st.view_rect.is_some_and(|r| {
            ui.ctx()
                .input(|i| i.pointer.interact_pos().is_some_and(|p| r.contains(p)))
        });

    // 只有「跟随中且有新内容」或用户点了「回到底部」时才滚动一次，
    // 每帧无条件贴底会把用户正在查看的历史日志抢走
    let stick = ((st.follow && has_new) || st.jump) && !pointer_busy;

    let out = egui::ScrollArea::vertical()
        .id_salt("log_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut shown = 0usize;
            for log in entries {
                if !st.filter.accepts(log.level) {
                    continue;
                }
                shown += 1;
                log_row(ui, log);
            }
            if shown == 0 {
                let p = theme::palette(ui);
                ui.add_space(theme::SPACE_SM);
                let hint = if st.filter == LogFilter::All {
                    "暂无日志 —— 开始发送后，这里会按时间、等级显示运行记录".to_string()
                } else {
                    format!(
                        "当前筛选（{}）下暂无日志 —— 可切换到「全部」查看完整记录",
                        st.filter.label()
                    )
                };
                ui.label(
                    egui::RichText::new(hint)
                        .size(11.5)
                        .color(p.text_secondary),
                );
            }
            if stick {
                ui.scroll_to_cursor_animation(
                    Some(egui::Align::BOTTOM),
                    egui::style::ScrollAnimation::none(),
                );
            }
        });

    // 用实际滚动偏移判断用户是否在查看历史：
    // - 用户自己把位置移到/移回底部 → 暂停 / 恢复跟随
    // - 鼠标按住日志区（选中文本/拖动） → 暂停跟随
    let max_offset = (out.content_size.y - out.inner_rect.height()).max(0.0);
    let offset = out.state.clamped_offset().y;
    let at_bottom = max_offset - offset <= 1.0;
    let offset_changed = (offset - st.last_offset).abs() > 0.5;
    // 内容不足一屏时永远视为「在底部」
    let fits = max_offset <= 0.0;

    if fits || st.jump {
        st.follow = true;
    } else if offset_changed {
        // 用户自己把滚动位置移到了底部 → 恢复跟随；移到中间 → 暂停
        st.follow = at_bottom;
    }
    // 鼠标按住日志区（拖滚动条 / 选中文本）期间一律暂停跟随，
    // 松开后保持暂停，直到用户滚回底部或点「回到底部」
    if pointer_busy {
        st.follow = false;
    }

    st.jump = false;
    st.last_offset = offset;
    st.seen = entries.len();
    st.view_rect = Some(out.inner_rect);
}

/// 结构化日志行：等宽字体 + 固定时间列 + 固定等级列 + 单行省略（hover 看全文）
fn log_row(ui: &mut egui::Ui, log: &LogItem) {
    let p = theme::palette(ui);
    let (level_text, level_color, msg_color) = match log.level {
        LogLevel::Info => ("INFO", p.text_secondary, p.text),
        LogLevel::Warn => ("WARN", p.warn, p.warn),
        LogLevel::Error => ("ERROR", p.danger, p.danger),
    };

    ui.horizontal(|ui| {
        ui.add_sized(
            [58.0, 18.0],
            egui::Label::new(
                egui::RichText::new(&log.time)
                    .monospace()
                    .size(11.5)
                    .color(p.text_secondary),
            ),
        );
        ui.add_sized(
            [44.0, 18.0],
            egui::Label::new(
                egui::RichText::new(level_text)
                    .monospace()
                    .size(11.0)
                    .color(level_color),
            ),
        );
        let width = ui.available_width().max(48.0);
        let resp = ui.add_sized(
            [width, 18.0],
            egui::Label::new(
                egui::RichText::new(&log.message)
                    .monospace()
                    .size(11.5)
                    .color(msg_color),
            )
            .truncate(),
        );
        // 长消息单行省略，悬停显示全文
        resp.on_hover_text(&log.message);
    });
}

/// 日志级别对应的颜色（状态条圆点、日志行共用），取自当前主题的语义色
fn level_color(ui: &egui::Ui, level: LogLevel) -> egui::Color32 {
    let p = theme::palette(ui);
    match level {
        LogLevel::Info => p.text_secondary,
        LogLevel::Warn => p.warn,
        LogLevel::Error => p.danger,
    }
}

/// 状态条右侧的发送状态摘要（等待倒计时 / 分卷进度 / 正在停止）
fn progress_summary(p: &SendProgress) -> String {
    if p.is_waiting() {
        format!("等待 · 剩余 {}", format_left(p.waiting_left()))
    } else if p.is_stopped() {
        "正在停止…".to_string()
    } else {
        format!(
            "文件 {}/{} · 卷 {}/{}",
            p.current_file_index.load(Ordering::Relaxed),
            p.total_files,
            p.current_file_sent_parts.load(Ordering::Relaxed),
            p.current_file_total_parts.load(Ordering::Relaxed).max(1)
        )
    }
}

/// 分区卡片：统一的发送页/设置页分组样式
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(egui::CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(egui::Margin {
            left: theme::SPACE_LG as i8,
            right: theme::SPACE_LG as i8,
            top: theme::SPACE_MD as i8,
            bottom: theme::SPACE_MD as i8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).size(13.5).color(p.text));
            ui.add_space(theme::SPACE_SM);
            add(ui);
        });
    ui.add_space(10.0);
}

// ======================= 图标 / 按钮体系 =======================

/// 单色矢量图标（16px 绘制档，按钮内统一 16px，页面头部 20px）
#[derive(Clone, Copy, PartialEq, Eq)]
enum Icon {
    Plus,
    Refresh,
    Close,
    Trash,
    Play,
    Pause,
    Stop,
    ArrowDown,
    Log,
    Save,
    Edit,
}

/// 按钮三级：主操作 / 次操作 / 轻操作（无底色）
#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnKind {
    Primary,
    Secondary,
    Ghost,
}

/// 画一个 16px 单色图标（用 Painter 画线，避免依赖 emoji / 图标字体）
fn paint_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: egui::Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height());
    let stroke = egui::Stroke::new(1.3, color);
    match icon {
        Icon::Plus => {
            let h = s * 0.30;
            painter.line_segment(
                [
                    egui::pos2(c.x - h, c.y),
                    egui::pos2(c.x + h, c.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, c.y - h),
                    egui::pos2(c.x, c.y + h),
                ],
                stroke,
            );
        }
        Icon::Close => {
            let h = s * 0.26;
            painter.line_segment(
                [
                    egui::pos2(c.x - h, c.y - h),
                    egui::pos2(c.x + h, c.y + h),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - h, c.y + h),
                    egui::pos2(c.x + h, c.y - h),
                ],
                stroke,
            );
        }
        Icon::Trash => {
            let w = s * 0.24;
            let top = c.y - s * 0.30;
            painter.line_segment(
                [
                    egui::pos2(c.x - w * 1.5, top),
                    egui::pos2(c.x + w * 1.5, top),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - w * 0.4, top - s * 0.10),
                    egui::pos2(c.x + w * 0.4, top - s * 0.10),
                ],
                stroke,
            );
            let body = egui::Rect::from_min_max(
                egui::pos2(c.x - w, top + 3.0),
                egui::pos2(c.x + w, c.y + s * 0.32),
            );
            painter.rect_stroke(
                body,
                egui::CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, top + 6.0),
                    egui::pos2(c.x, body.bottom() - 3.0),
                ],
                stroke,
            );
        }
        Icon::Play => {
            let h = s * 0.30;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - h * 0.7, c.y - h),
                    egui::pos2(c.x + h * 0.9, c.y),
                    egui::pos2(c.x - h * 0.7, c.y + h),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Icon::Pause => {
            let h = s * 0.28;
            let w = s * 0.09;
            for dx in [-w * 1.6, w * 1.6] {
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        egui::pos2(c.x + dx, c.y),
                        egui::vec2(w * 1.4, h * 2.0),
                    ),
                    egui::CornerRadius::same(1),
                    color,
                );
            }
        }
        Icon::Stop => {
            let h = s * 0.22;
            painter.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(h * 2.0, h * 2.0)),
                egui::CornerRadius::same(2),
                color,
            );
        }
        Icon::ArrowDown => {
            let h = s * 0.30;
            painter.line_segment(
                [
                    egui::pos2(c.x, c.y - h),
                    egui::pos2(c.x, c.y + h),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - h * 0.8, c.y + h * 0.25),
                    egui::pos2(c.x, c.y + h),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x + h * 0.8, c.y + h * 0.25),
                    egui::pos2(c.x, c.y + h),
                ],
                stroke,
            );
        }
        Icon::Log => {
            let w = s * 0.28;
            for i in -1..=1 {
                let y = c.y + i as f32 * s * 0.20;
                painter.line_segment(
                    [
                        egui::pos2(c.x - w, y),
                        egui::pos2(c.x + w, y),
                    ],
                    stroke,
                );
            }
        }
        Icon::Save => {
            let w = s * 0.26;
            let h = s * 0.30;
            painter.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(w * 2.0, h * 2.0)),
                egui::CornerRadius::same(2),
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - w * 0.9, c.y - h),
                    egui::pos2(c.x + w * 0.5, c.y - h),
                ],
                stroke,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(c.x - w * 0.6, c.y + h * 0.15),
                    egui::pos2(c.x + w * 0.6, c.y + h),
                ),
                egui::CornerRadius::same(1),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        Icon::Edit => {
            let h = s * 0.28;
            painter.line_segment(
                [
                    egui::pos2(c.x - h, c.y + h),
                    egui::pos2(c.x + h * 0.8, c.y - h * 0.8),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - h, c.y + h),
                    egui::pos2(c.x - h * 0.2, c.y + h),
                ],
                stroke,
            );
        }
        Icon::Refresh => {
            let r = s * 0.30;
            let mut pts = Vec::new();
            for i in 0..=9 {
                let a = -0.55 + i as f32 * 0.60;
                pts.push(egui::pos2(c.x + r * a.cos(), c.y + r * a.sin()));
            }
            if pts.len() >= 2 {
                let last = pts[pts.len() - 1];
                painter.add(egui::Shape::line(pts, stroke));
                painter.line_segment(
                    [last, last + egui::vec2(-r * 0.55, r * 0.25)],
                    stroke,
                );
                painter.line_segment(
                    [last, last + egui::vec2(-r * 0.15, -r * 0.55)],
                    stroke,
                );
            }
        }
    }
}

/// 颜色加透明度（用于禁用态 / 语义色底色）
fn tint(color: egui::Color32, alpha: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// 两色线性混合（用于开关轨道、进度等过渡）
fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    egui::Color32::from_rgba_unmultiplied(
        (a.r() as f32 * (1.0 - t) + b.r() as f32 * t) as u8,
        (a.g() as f32 * (1.0 - t) + b.g() as f32 * t) as u8,
        (a.b() as f32 * (1.0 - t) + b.b() as f32 * t) as u8,
        (a.a() as f32 * (1.0 - t) + b.a() as f32 * t) as u8,
    )
}

fn button(ui: &mut egui::Ui, kind: BtnKind, icon: Option<Icon>, text: &str) -> egui::Response {
    button_tinted(ui, kind, icon, text, None, None)
}

/// 自绘按钮：primary / secondary / ghost 三级；可选 16px 图标 + 可选覆盖底色
fn button_tinted(
    ui: &mut egui::Ui,
    kind: BtnKind,
    icon: Option<Icon>,
    text: &str,
    fill_override: Option<egui::Color32>,
    ink_override: Option<egui::Color32>,
) -> egui::Response {
    let p = theme::palette(ui);
    let enabled = ui.is_enabled();
    let height = 28.0;

    let ink = match kind {
        BtnKind::Primary => ink_override.unwrap_or(p.accent_ink),
        BtnKind::Secondary => p.text,
        BtnKind::Ghost => p.text_secondary,
    };
    let ink = if enabled { ink } else { tint(ink, 110) };

    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(13.0), ink);

    let has_icon = icon.is_some();
    let icon_w = if has_icon { 16.0 } else { 0.0 };
    let gap = if has_icon && !text.is_empty() { 6.0 } else { 0.0 };
    let pad_x = if text.is_empty() { 6.0 } else { 11.0 };
    let width = if text.is_empty() {
        if has_icon {
            icon_w + pad_x * 2.0
        } else {
            20.0
        }
    } else {
        (galley.size().x + icon_w + gap + pad_x * 2.0).max(56.0)
    };

    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();

    let radius = egui::CornerRadius::same(theme::CONTROL_RADIUS);
    let painter = ui.painter();

    let (bg, border) = match kind {
        BtnKind::Primary => (fill_override.unwrap_or(p.accent), None),
        BtnKind::Secondary => (
            if hovered { p.hover } else { p.surface_alt },
            Some(if hovered { p.border_hover } else { p.border }),
        ),
        BtnKind::Ghost => (
            if hovered { p.hover } else { egui::Color32::TRANSPARENT },
            None,
        ),
    };
    let bg = if enabled { bg } else { tint(bg, 100) };
    painter.rect_filled(rect, radius, bg);
    if let Some(b) = border {
        painter.rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, if enabled { b } else { tint(b, 120) }),
            egui::StrokeKind::Inside,
        );
    }
    if pressed {
        painter.rect_filled(rect, radius, egui::Color32::from_black_alpha(22));
    }

    let content_w = galley.size().x + icon_w + gap;
    let start_x = rect.center().x - content_w * 0.5;
    if let Some(icon) = icon {
        paint_icon(
            painter,
            egui::Rect::from_center_size(
                egui::pos2(start_x + icon_w * 0.5, rect.center().y),
                egui::vec2(16.0, 16.0),
            ),
            icon,
            ink,
        );
    }
    painter.galley(
        egui::pos2(start_x + icon_w + gap, rect.center().y - galley.size().y * 0.5),
        galley,
        ink,
    );

    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// 主操作 CTA（132×34，发送页底栏用）
fn button_cta(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let p = theme::palette(ui);
    let ink = if enabled { p.accent_ink } else { p.text_secondary };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(13.5), ink);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(132.0, 34.0), sense);
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(theme::CONTROL_RADIUS);
    painter.rect_filled(
        rect,
        radius,
        if enabled { p.accent } else { p.surface_alt },
    );
    if enabled && resp.hovered() {
        painter.rect_filled(rect, radius, egui::Color32::from_black_alpha(16));
    }
    if enabled && resp.is_pointer_button_down_on() {
        painter.rect_filled(rect, radius, egui::Color32::from_black_alpha(30));
    }
    painter.galley(rect.center() - galley.size() * 0.5, galley, ink);

    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// 只读小徽标（不可点击）
fn chip(ui: &mut egui::Ui, text: &str, fg: egui::Color32, bg: egui::Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.5), fg);
    let pad = egui::vec2(8.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(5), bg);
    painter.galley(rect.min + pad, galley, fg);
}

/// 可点击 chip（等级筛选等）
fn chip_button(ui: &mut egui::Ui, text: &str, selected: bool) -> egui::Response {
    let p = theme::palette(ui);
    let (fg, bg) = if selected {
        (p.accent, p.accent_soft)
    } else {
        (p.text_secondary, p.surface_alt)
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.5), fg);
    let pad = egui::vec2(9.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(5),
        if !selected && resp.hovered() {
            p.hover
        } else {
            bg
        },
    );
    if selected {
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(5),
            egui::Stroke::new(1.0, tint(p.accent, 90)),
            egui::StrokeKind::Inside,
        );
    }
    painter.galley(rect.min + pad, galley, fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 表单标签列（统一 12px 次级色）
fn field_label(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette(ui);
    ui.label(egui::RichText::new(text).size(12.0).color(p.text_secondary));
}

/// 开关（36×20，knob 用 animate_bool 平滑滑动）
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let p = theme::palette(ui);
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(36.0, 20.0), egui::Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.2);
    let painter = ui.painter();
    let track = mix(p.surface_alt, p.accent, t);
    painter.rect_filled(rect, egui::CornerRadius::same(10), track);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(10),
        egui::Stroke::new(1.0, if resp.hovered() { p.border_hover } else { p.border }),
        egui::StrokeKind::Inside,
    );
    let knob_x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    painter.circle_filled(
        egui::pos2(knob_x, rect.center().y),
        7.0,
        if t > 0.5 { p.accent_ink } else { p.text_secondary },
    );
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 日期 DragValue（按天偏移，显示为 YYYY-MM-DD）
fn date_drag(ui: &mut egui::Ui, date: &mut (i32, u32, u32)) -> bool {
    let today = chrono::Local::now().date_naive();
    let current = chrono::NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap_or(today);
    let mut offset = current.signed_duration_since(today).num_days() as i32;

    let resp = ui.add_sized(
        [104.0, 28.0],
        egui::DragValue::new(&mut offset)
            .range(-3650..=3650)
            .speed(1.0)
            .custom_formatter(move |n, _| {
                (today + chrono::Duration::days(n as i64))
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .custom_parser(move |s| {
                chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                    .ok()
                    .map(|d| d.signed_duration_since(today).num_days() as f64)
            }),
    );

    if resp.changed() {
        if let Some(d) = today.checked_add_signed(chrono::Duration::days(offset as i64)) {
            use chrono::Datelike;
            *date = (d.year(), d.month(), d.day());
        }
    }
    resp.changed()
}

/// 时间 DragValue（按分钟偏移，显示为 HH:MM）
fn time_drag(ui: &mut egui::Ui, time: &mut (u32, u32, u32)) -> bool {
    let mut minutes = time.0 * 60 + time.1;
    let resp = ui.add_sized(
        [72.0, 28.0],
        egui::DragValue::new(&mut minutes)
            .range(0..=1439)
            .speed(1.0)
            .custom_formatter(|n, _| {
                let total = n as u32;
                format!("{:02}:{:02}", total / 60, total % 60)
            })
            .custom_parser(|s| {
                let (h, m) = s.trim().split_once(':')?;
                Some(h.trim().parse::<f64>().ok()? * 60.0 + m.trim().parse::<f64>().ok()?)
            }),
    );
    if resp.changed() {
        time.0 = minutes / 60;
        time.1 = minutes % 60;
    }
    resp.changed()
}

/// 附件拖放空态：虚线区，拖入高亮，点击打开文件选择
fn drop_zone(ui: &mut egui::Ui) -> bool {
    let p = theme::palette(ui);
    let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 88.0),
        egui::Sense::click(),
    );

    let stroke_color = if hovering { p.accent } else { p.border_hover };
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(theme::CARD_RADIUS),
        if hovering {
            p.accent_soft
        } else {
            p.surface_alt
        },
    );
    let inner = rect.shrink(1.0);
    let corners = [
        inner.left_top(),
        inner.right_top(),
        inner.right_bottom(),
        inner.left_bottom(),
    ];
    let dash = egui::Stroke::new(1.0, stroke_color);
    for i in 0..4 {
        painter.extend(egui::Shape::dashed_line(
            &[corners[i], corners[(i + 1) % 4]],
            dash,
            7.0,
            5.0,
        ));
    }

    let center = rect.center();
    paint_icon(
        painter,
        egui::Rect::from_center_size(
            egui::pos2(center.x, center.y - 22.0),
            egui::vec2(16.0, 16.0),
        ),
        Icon::ArrowDown,
        if hovering { p.accent } else { p.text_secondary },
    );
    painter.text(
        egui::pos2(center.x, center.y - 3.0),
        egui::Align2::CENTER_CENTER,
        "拖放文件到此处",
        egui::FontId::proportional(13.5),
        p.text,
    );
    painter.text(
        egui::pos2(center.x, center.y + 15.0),
        egui::Align2::CENTER_CENTER,
        "或点击此区域选择文件",
        egui::FontId::proportional(11.0),
        p.text_secondary,
    );

    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// 附件类型徽标（最多 4 个字符，按类别着色）
fn type_badge(ui: &mut egui::Ui, path: &std::path::Path) {
    let p = theme::palette(ui);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let label = if ext.is_empty() {
        "FILE".to_string()
    } else {
        ext.chars().take(4).collect::<String>().to_ascii_uppercase()
    };
    let color = match ext.as_str() {
        "7z" | "zip" | "rar" | "tar" | "gz" => p.warn,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" => p.accent,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => p.ok,
        _ => p.text_secondary,
    };

    let (rect, _) = ui.allocate_exact_size(egui::vec2(40.0, 20.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(5), tint(color, 30));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(10.0),
        color,
    );
}

/// 单个附件行：类型徽标 + 文件名 + 体积 + 删除（不显示绝对路径）
fn attachment_row(ui: &mut egui::Ui, path: &std::path::Path) -> bool {
    let p = theme::palette(ui);
    let mut deleted = false;
    egui::Frame::new()
        .fill(p.surface_alt)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
        .inner_margin(egui::Margin {
            left: 8,
            right: 6,
            top: 5,
            bottom: 5,
        })
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                type_badge(ui, path);
                ui.add_space(theme::SPACE_SM);

                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("未知文件");
                let name_w = (ui.available_width() - 100.0).max(70.0);
                ui.add_sized(
                    [name_w, 18.0],
                    egui::Label::new(
                        egui::RichText::new(name).size(12.5).color(p.text),
                    )
                    .truncate(),
                );

                let size = std::fs::metadata(path)
                    .map(|m| autofill::format_size(m.len()))
                    .unwrap_or_else(|_| "—".to_string());
                ui.add_sized(
                    [56.0, 18.0],
                    egui::Label::new(
                        egui::RichText::new(size).size(11.0).color(p.text_secondary),
                    )
                    .truncate(),
                );

                if button(ui, BtnKind::Ghost, Some(Icon::Close), "").clicked() {
                    deleted = true;
                }
            });
        });
    deleted
}

/// 账号列表行：首字符头像 + host:port · SSL · 登录名 + 状态徽标 + 次级小按钮
fn account_row(ui: &mut egui::Ui, acc: &SmtpAccount) -> Option<AccountRowAction> {
    let p = theme::palette(ui);
    let mut action = None;

    egui::Frame::new()
        .fill(p.surface_alt)
        .stroke(egui::Stroke::new(1.0, p.border))
        .corner_radius(egui::CornerRadius::same(theme::CONTROL_RADIUS))
        .inner_margin(egui::Margin {
            left: 10,
            right: 6,
            top: 7,
            bottom: 7,
        })
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                // 首字符头像
                let (avatar_rect, _) =
                    ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(
                    avatar_rect,
                    egui::CornerRadius::same(8),
                    p.accent_soft,
                );
                let initial = acc
                    .name
                    .chars()
                    .next()
                    .or_else(|| acc.username.chars().next())
                    .unwrap_or('•')
                    .to_uppercase()
                    .to_string();
                painter.text(
                    avatar_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    initial,
                    egui::FontId::proportional(13.0),
                    p.accent,
                );

                ui.add_space(theme::SPACE_SM);
                let text_w = (ui.available_width() - 228.0).max(90.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(text_w, 38.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let name = if acc.name.trim().is_empty() {
                            "未命名账号"
                        } else {
                            acc.name.as_str()
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(name).size(12.5).color(p.text),
                            )
                            .truncate(),
                        );
                        let host = if acc.smtp.trim().is_empty() {
                            "未填写服务器".to_string()
                        } else {
                            format!("{}:{}", acc.smtp, acc.port)
                        };
                        let ssl = if acc.ssl { "SSL" } else { "明文" };
                        let user = if acc.username.trim().is_empty() {
                            "未填写登录名"
                        } else {
                            acc.username.as_str()
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{} · {} · {}", host, ssl, user))
                                    .size(10.5)
                                    .color(p.text_secondary),
                            )
                            .truncate(),
                        );
                    },
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, BtnKind::Ghost, Some(Icon::Trash), "删除").clicked() {
                        action = Some(AccountRowAction::Delete);
                    }
                    if button(ui, BtnKind::Ghost, Some(Icon::Edit), "编辑").clicked() {
                        action = Some(AccountRowAction::Edit);
                    }
                    status_badge(ui, acc);
                });
            });
        });

    action
}

/// 账号状态徽标：SMTP / 登录名 / 密码都填写才视为「可用」
fn status_badge(ui: &mut egui::Ui, acc: &SmtpAccount) {
    let p = theme::palette(ui);
    let ready = !acc.smtp.trim().is_empty()
        && !acc.username.trim().is_empty()
        && !acc.password.trim().is_empty();
    if ready {
        chip(ui, "可用", p.ok, tint(p.ok, 30));
    } else {
        chip(ui, "待完善", p.warn, tint(p.warn, 30));
    }
}

/// 分段控件：track + 选中 pill（自绘，避免默认按钮在窄窗口挤压换行）；返回是否切换页签
fn tab_switch(ui: &mut egui::Ui, tab: &mut AppTab, p: theme::Palette, width: f32) -> bool {
    let height = 30.0;
    let (track, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(track, egui::CornerRadius::same(height as u8 / 2), p.surface_alt);
    painter.rect_stroke(
        track,
        egui::CornerRadius::same(height as u8 / 2),
        egui::Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );

    let pad = 3.0;
    let pill_h = height - pad * 2.0;
    let seg_w = (width - pad * 2.0) / 2.0;
    let tabs = [(AppTab::Send, "发送"), (AppTab::Settings, "设置")];
    let mut changed = false;

    for (i, (value, label)) in tabs.into_iter().enumerate() {
        let x = track.left() + pad + seg_w * i as f32;
        let pill = egui::Rect::from_min_size(
            egui::pos2(x, track.top() + pad),
            egui::vec2(seg_w, pill_h),
        );
        let resp = ui.interact(
            pill,
            ui.scope_id().with(("tab_switch", i)),
            egui::Sense::click(),
        );
        let selected = *tab == value;
        let radius = egui::CornerRadius::same((pill_h as u8) / 2);

        if selected {
            painter.rect_filled(pill, radius, p.accent_soft);
        } else if resp.hovered() {
            painter.rect_filled(pill, radius, p.hover);
        }

        let color = if selected { p.accent } else { p.text_secondary };
        painter.text(
            pill.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.5),
            color,
        );

        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && *tab != value {
            *tab = value;
            changed = true;
        }
    }
    changed
}

/// 剩余秒数格式化为「X 天 HH:MM:SS」
fn format_left(secs: u64) -> String {
    let d = secs / 86_400;
    let h = secs % 86_400 / 3600;
    let m = secs % 3600 / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{} 天 {:02}:{:02}:{:02}", d, h, m, s)
    } else {
        format!("{:02}:{:02}:{:02}", h, m, s)
    }
}
