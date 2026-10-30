use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

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

/// 主窗口底部状态条高度（只放一行：最新日志 + 计数 + 打开日志按钮）
const STATUS_BAR_HEIGHT: f32 = 28.0;

/// 日志窗口初始尺寸 / 最小尺寸
const LOG_WINDOW_SIZE: [f32; 2] = [720.0, 520.0];
const LOG_WINDOW_MIN_SIZE: [f32; 2] = [480.0, 320.0];

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
}

impl LogUiState {
    fn new() -> Self {
        Self {
            follow: true,
            ..Default::default()
        }
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
}

#[derive(PartialEq)]
enum AppTab {
    Send,
    Settings,
}

enum AccountEditAction {
    Save,
    Cancel,
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
        }
    }
}

impl eframe::App for MailApp {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("📧 MailSender");
                ui.separator();
                ui.selectable_value(&mut self.tab, AppTab::Send, "发送");
                ui.selectable_value(&mut self.tab, AppTab::Settings, "设置");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.sending {
                        if let Some(p) = &self.progress {
                            if p.is_waiting() {
                                ui.colored_label(
                                    egui::Color32::DARK_GRAY,
                                    format!("⏳ 等待定时（{}）", format_left(p.waiting_left())),
                                );
                            } else if p.is_stopped() {
                                ui.colored_label(egui::Color32::RED, "正在停止…");
                            } else {
                                ui.label("发送中…");
                            }
                        }
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

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            AppTab::Send => self.ui_send(ui),
            AppTab::Settings => self.ui_settings(ui),
        });

        // 日志窗口：先处理本帧的「打开/前置」请求，再在需要时重新声明该视口。
        // 必须在 UI 之后做，这样本帧按钮点击产生的请求也能立刻生效。
        if self.pending_open_log {
            self.pending_open_log = false;
            self.request_log_window(&ctx);
        }
        if self.log_open.load(Ordering::Relaxed) {
            self.show_log_window(&ctx);
        }

        // 只在需要刷新时请求重绘：发送中刷新进度/倒计时/日志窗口，发送页勾选定时后刷新预览；
        // 空闲时不再固定每 200ms 重绘，避免无意义的 UI 空转。
        if self.sending {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
            if self.log_open.load(Ordering::Relaxed) {
                // 后台线程写日志不会唤醒 UI，这里按节奏唤醒日志子窗口
                ctx.request_repaint_after_for(
                    std::time::Duration::from_millis(200),
                    log_viewport_id(),
                );
            }
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
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("📂 添加文件").min_size(egui::vec2(110.0, 28.0)))
                        .clicked()
                    {
                        if let Some(mut files) = rfd::FileDialog::new().pick_files() {
                            self.files.append(&mut files);
                            self.refresh_autofill();
                        }
                    }
                    if !self.files.is_empty() && ui.button("清空").clicked() {
                        self.files.clear();
                        self.refresh_autofill();
                    }
                    if !self.files.is_empty()
                        && ui
                            .button("✨ 根据附件重新生成主题/正文")
                            .on_hover_text("覆盖当前主题与正文（包括手动修改过的内容）")
                            .clicked()
                    {
                        self.subject_dirty = false;
                        self.body_dirty = false;
                        self.refresh_autofill();
                        self.logger.info("已根据附件重新生成主题与正文");
                    }
                });

                if self.files.is_empty() {
                    ui.weak("尚未选择文件");
                } else {
                    let mut remove: Option<usize> = None;
                    for (i, f) in self.files.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                            ui.label(f.display().to_string());
                        });
                    }
                    if let Some(i) = remove {
                        self.files.remove(i);
                        self.refresh_autofill();
                    }

                    if let Some(sum) = autofill::summarize_attachments(&self.files) {
                        ui.weak(format!(
                            "共 {} 个文件，总大小 {}",
                            sum.count,
                            autofill::format_size(sum.total_bytes)
                        ));
                    }
                }
            });

            // ---------- ② 收件人与内容 ----------
            section(ui, "② 收件人与内容", |ui| {
                egui::Grid::new("send_fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("收件人");
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
                                    if ui.selectable_label(false, email).clicked() {
                                        self.to = email.clone();
                                    }
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("主题");
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

                    ui.label("正文");
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
                        .add(
                            egui::DragValue::new(&mut min)
                                .range(0..=86_400u64)
                                .prefix("最小 ")
                                .suffix(" 秒"),
                        )
                        .changed();
                    let r2 = ui
                        .add(
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
                    ui.colored_label(egui::Color32::RED, "最小值大于最大值，发送时会自动交换");
                }

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // ===== 定时开始（仅本次发送）=====
                let sched_resp =
                    ui.checkbox(&mut self.sched_enabled, "⏰ 定时开始（仅本次发送有效）");
                // 勾选时：用户还没手动选过时间，就重置为「当前本地时间 + 30 分钟」
                if sched_resp.changed() && self.sched_enabled && !self.sched_user_set {
                    self.reset_schedule_default();
                }

                ui.add_enabled_ui(self.sched_enabled, |ui| {
                    let mut edited = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label("开始时间");
                        edited |= ui
                            .add(
                                egui::DragValue::new(&mut self.sched_date.0)
                                    .range(2024..=2100)
                                    .suffix(" 年"),
                            )
                            .changed();
                        edited |= ui
                            .add(egui::DragValue::new(&mut self.sched_date.1).range(1..=12).suffix(" 月"))
                            .changed();
                        edited |= ui
                            .add(egui::DragValue::new(&mut self.sched_date.2).range(1..=31).suffix(" 日"))
                            .changed();
                        edited |= ui
                            .add(egui::DragValue::new(&mut self.sched_time.0).range(0..=23).suffix(" 时"))
                            .changed();
                        edited |= ui
                            .add(egui::DragValue::new(&mut self.sched_time.1).range(0..=59).suffix(" 分"))
                            .changed();
                        edited |= ui
                            .add(egui::DragValue::new(&mut self.sched_time.2).range(0..=59).suffix(" 秒"))
                            .changed();
                    });
                    if edited {
                        // 手动改过任何一个字段后，后续勾选/取消不再覆盖用户选择
                        self.sched_user_set = true;
                    }
                    // 顺序：−1h −10m −5m | +1m +10m +1h；窄窗口自动换行避免挤爆
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("−1 小时").clicked() {
                            self.shift_schedule(Duration::hours(-1));
                        }
                        if ui.button("−10 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(-10));
                        }
                        if ui.button("−5 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(-5));
                        }
                        ui.separator();
                        if ui.button("+1 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(1));
                        }
                        if ui.button("+10 分钟").clicked() {
                            self.shift_schedule(Duration::minutes(10));
                        }
                        if ui.button("+1 小时").clicked() {
                            self.shift_schedule(Duration::hours(1));
                        }
                    });

                    match self.schedule_target() {
                        Ok(t) => {
                            let now = chrono::Local::now().naive_local();
                            let left = (t - now).num_seconds();
                            if t > now {
                                ui.label(format!(
                                    "将于 {} 开始（剩余 {}）",
                                    t.format("%Y-%m-%d %H:%M:%S"),
                                    format_left(left as u64)
                                ));
                            } else {
                                ui.colored_label(
                                    egui::Color32::DARK_BLUE,
                                    format!(
                                        "选定时间 {} 已过，点击开始后会立即发送",
                                        t.format("%Y-%m-%d %H:%M:%S")
                                    ),
                                );
                            }
                        }
                        Err(e) => {
                            ui.colored_label(egui::Color32::RED, format!("开始时间无效: {e}"));
                        }
                    }
                });
            });

            // ---------- ④ 开始发送 / 进度 ----------
            ui.add_space(6.0);
            if !self.sending {
                let ready = !self.files.is_empty()
                    && !self.to.trim().is_empty()
                    && !self.config.accounts.is_empty();
                let btn = egui::Button::new("🚀 开始发送").min_size(egui::vec2(160.0, 34.0));
                if ui.add_enabled(ready, btn).clicked() {
                    self.start_send();
                }
                if !ready {
                    ui.weak("需要先选择附件、填写收件人，并在「设置」页配置 SMTP 账号");
                }
            } else if let Some(p) = &self.progress {
                self.ui_progress(ui, p.clone());
            }
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

    fn ui_progress(&mut self, ui: &mut egui::Ui, progress: Arc<SendProgress>) {
        // 定时等待阶段：进度条还没意义，展示倒计时
        if progress.is_waiting() {
            ui.colored_label(
                egui::Color32::DARK_GRAY,
                format!("⏳ 等待定时开始，剩余 {}", format_left(progress.waiting_left())),
            );
            ui.horizontal(|ui| {
                if ui.button("⛔ 取消等待").clicked() {
                    progress.stopped.store(true, Ordering::Relaxed);
                }
            });
            return;
        }

        let cur_file = progress.current_file_index.load(Ordering::Relaxed);
        let total_files = progress.total_files;

        let file_ratio = cur_file as f32 / total_files.max(1) as f32;

        let part_ratio = progress.current_file_sent_parts.load(Ordering::Relaxed) as f32
            / progress
                .current_file_total_parts
                .load(Ordering::Relaxed)
                .max(1) as f32;

        ui.label(format!("文件进度: {}/{}", cur_file, total_files));
        ui.add(egui::ProgressBar::new(file_ratio).show_percentage());

        ui.label("当前文件分卷进度");
        ui.add(egui::ProgressBar::new(part_ratio).show_percentage());

        if progress.is_stopped() {
            ui.colored_label(egui::Color32::RED, "正在停止…");
        }

        ui.horizontal(|ui| {
            if ui.button("⏸ 暂停").clicked() {
                progress.paused.store(true, Ordering::Relaxed);
            }
            if ui.button("▶ 继续").clicked() {
                progress.paused.store(false, Ordering::Relaxed);
            }
            if ui.button("⛔ 停止").clicked() {
                // 只置停止标记，等后台线程真正结束后由 ui() 复位 sending
                progress.stopped.store(true, Ordering::Relaxed);
            }
        });
    }

    fn start_send(&mut self) {
        if self.files.is_empty() {
            self.logger.error("未选择文件，无法发送");
            return;
        }
        if self.config.accounts.is_empty() {
            self.logger.error("没有可用的 SMTP 账号，无法发送");
            return;
        }
        if self.to.trim().is_empty() {
            self.logger.error("收件人为空，无法发送");
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
        self.schedule_used_this_run = schedule_at.is_some();
        // 开始发送时自动打开日志窗口（已打开则只做前置，不会多开窗口）
        self.pending_open_log = true;
        self.config.history.record(&to);
        thread::spawn(move || {
            start_send_task(&files, &to, &subject, &body, &config, schedule_at, progress, logger);
        });
    }

    /* =================== 设置页 =================== */

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            section(ui, "SMTP 账号", |ui| {
                let mut remove_index: Option<usize> = None;
                let mut start_edit: Option<(Option<usize>, SmtpAccount)> = None;

                if self.config.accounts.is_empty() {
                    ui.weak("尚未配置账号，请先添加");
                }
                for (i, acc) in self.config.accounts.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}（{}:{}）", acc.name, acc.smtp, acc.port));
                        if ui.button("编辑").clicked() {
                            start_edit = Some((Some(i), acc.clone()));
                        }
                        if ui.button("删除").clicked() {
                            remove_index = Some(i);
                        }
                    });
                }

                if let Some(i) = remove_index {
                    let removed_acc = self.config.accounts.remove(i);
                    if Some(removed_acc.id) == self.edit_account_id {
                        self.edit_account_id = None;
                        self.edit_account = None;
                    }
                    self.save_config();
                }

                if ui.button("➕ 添加账号").clicked() {
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

                // ===== 进入编辑态（UI 后处理）=====
                if let Some((idx, acc)) = start_edit {
                    // 必须记录被编辑账号的 ID，否则保存时会被当成「新增」而重复插入账号
                    self.edit_account_id = if idx.is_some() { Some(acc.id) } else { None };
                    self.edit_account = Some(acc);
                }
            });

            // ===== 编辑面板 =====
            let mut action: Option<AccountEditAction> = None;

            if let Some(acc) = &mut self.edit_account {
                section(ui, "账号配置", |ui| {
                    egui::Grid::new("account_edit").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.label("账号名称");
                        ui.text_edit_singleline(&mut acc.name);
                        ui.end_row();
                        ui.label("SMTP 地址");
                        ui.text_edit_singleline(&mut acc.smtp);
                        ui.end_row();
                        ui.label("SMTP 端口");
                        ui.add(egui::DragValue::new(&mut acc.port).range(1..=65535));
                        ui.end_row();
                        ui.label("账号");
                        ui.text_edit_singleline(&mut acc.username);
                        ui.end_row();
                        ui.label("密码/授权码");
                        ui.add(egui::TextEdit::singleline(&mut acc.password).password(true));
                        ui.end_row();
                        ui.label("SSL");
                        ui.checkbox(&mut acc.ssl, "465 端口一般勾选；587 一般不勾");
                        ui.end_row();
                    });

                    ui.horizontal(|ui| {
                        if ui.button("💾 保存账号").clicked() {
                            action = Some(AccountEditAction::Save);
                        }
                        if ui.button("取消").clicked() {
                            action = Some(AccountEditAction::Cancel);
                        }
                    });
                });
            }

            // ===== 编辑结果处理 =====
            if let Some(action) = action {
                match action {
                    AccountEditAction::Save => {
                        if let Some(acc) = self.edit_account.take() {
                            match self.edit_account_id {
                                Some(id) => {
                                    if let Some(target) =
                                        self.config.accounts.iter_mut().find(|a| a.id == id)
                                    {
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
                    AccountEditAction::Cancel => {
                        self.edit_account = None;
                        self.edit_account_id = None;
                    }
                }
            }

            section(ui, "7z 分卷与加密", |ui| {
                ui.horizontal(|ui| {
                    ui.label("分卷大小");
                    ui.add(
                        egui::DragValue::new(&mut self.config.split_mb)
                            .range(1..=10_000u64)
                            .suffix(" MB"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("压缩密码");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.sevenz_password)
                            .hint_text("留空则不加密")
                            .desired_width(200.0),
                    );
                });
                if self.config.sevenz_password.is_empty() {
                    ui.colored_label(egui::Color32::YELLOW, "密码为空：附件将不加密发送");
                }
            });

            if ui
                .add(egui::Button::new("💾 保存配置").min_size(egui::vec2(120.0, 30.0)))
                .clicked()
            {
                for note in self.config.normalize() {
                    self.logger.warn(format!("配置已修正: {}", note));
                }
                self.save_config();
            }
        });
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

        // 右侧固定块宽度（打开日志 + 共 N 条 + ⤓ + 进度文字）
        let right_width = if self.sending { 300.0 } else { 200.0 };
        let left_width = (ui.available_width() - right_width).max(80.0);

        ui.horizontal_centered(|ui| {
            // 左侧：最新一条日志（宽度不足时截断）
            ui.allocate_ui_with_layout(
                egui::vec2(left_width, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| match entries.last() {
                    Some(log) => {
                        ui.label(egui::RichText::new("●").color(level_color(log.level)));
                        ui.add(
                            egui::Label::new(format!("[{}] {}", log.time, log.message))
                                .truncate(),
                        );
                    }
                    None => {
                        ui.weak("暂无日志");
                    }
                },
            );

            // 右侧：进度状态 / 跟随暂停 / 条数 / 打开日志（从右到左排）
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("打开日志")
                    .on_hover_text("打开独立的日志窗口（已打开则前置）")
                    .clicked()
                {
                    self.pending_open_log = true;
                }
                ui.label(format!("共 {} 条", total));
                if !follow {
                    ui.colored_label(egui::Color32::YELLOW, "⤓")
                        .on_hover_text("日志窗口已暂停自动跟随（可回到底部恢复）");
                }
                if self.sending {
                    if let Some(p) = &self.progress {
                        ui.label(progress_summary(p));
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

                log_header(ui, &mut st, entries.len());
                draw_log_scroll(ui, &entries, &mut st);

                // 跟随状态变化时唤醒主窗口，让状态条的 ⤓ 提示同步
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

/// 日志窗口标题行（共 N 条 + 暂停跟随时的「回到底部」）
fn log_header(ui: &mut egui::Ui, st: &mut LogUiState, total: usize) {
    ui.horizontal(|ui| {
        ui.strong("运行日志");
        ui.weak(format!("共 {} 条", total));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !st.follow {
                ui.colored_label(egui::Color32::YELLOW, "已暂停自动跟随");
                if ui
                    .button("⤓ 回到底部")
                    .on_hover_text("恢复自动滚动到最新日志")
                    .clicked()
                {
                    st.follow = true;
                    st.jump = true;
                }
            }
        });
    });
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
            for log in entries {
                ui.colored_label(level_color(log.level), format!("[{}] {}", log.time, log.message));
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

/// 日志级别对应的颜色（状态条圆点、日志行共用）
fn level_color(level: LogLevel) -> egui::Color32 {
    match level {
        LogLevel::Info => egui::Color32::LIGHT_GREEN,
        LogLevel::Warn => egui::Color32::YELLOW,
        LogLevel::Error => egui::Color32::RED,
    }
}

/// 状态条右侧的发送状态摘要（等待倒计时 / 分卷进度 / 正在停止）
fn progress_summary(p: &SendProgress) -> String {
    if p.is_waiting() {
        format!("⏳ 剩余 {}", format_left(p.waiting_left()))
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
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.strong(title);
        ui.add_space(4.0);
        add(ui);
    });
    ui.add_space(6.0);
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
