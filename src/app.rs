use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use eframe::egui;
use uuid::Uuid;

use crate::autofill;
use crate::config::{AppConfig, SmtpAccount};
use crate::logger::{LogLevel, Logger};
use crate::progress::SendProgress;
use crate::sender::start_send_task;

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
    /// 本次发送任务实际使用了定时（用于结束后复位）
    schedule_used_this_run: bool,

    /// 当前账号编辑缓存
    edit_account: Option<SmtpAccount>,
    /// 当前账号编辑缓存的索引
    edit_index: Option<usize>,
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
        // 处理策略：直接忽略该开关并记录迁移说明；旧的时间字符串仅作为
        // 时间选择控件的初始值提示，不会自动启用定时。
        if config.schedule_enabled {
            logger.warn(
                "检测到旧版「全局定时开始」配置，已按新语义忽略：定时现在是每次发送的一次性选项（时间已预填到发送页，未启用）"
                    .to_string(),
            );
            config.schedule_enabled = false;
            config.save();
        }

        // 定时控件默认值：明天同一时间；若旧配置里有合法时间则预填
        let tomorrow = chrono::Local::now().naive_local() + Duration::days(1);
        let mut sched_date = (tomorrow.year(), tomorrow.month(), tomorrow.day());
        let mut sched_time = (tomorrow.hour(), tomorrow.minute(), 0u32);
        if let Ok(Some(t)) = config.parse_schedule_start() {
            sched_date = (t.year(), t.month(), t.day());
            sched_time = (t.hour(), t.minute(), t.second());
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
            schedule_used_this_run: false,
            edit_account: None,
            edit_index: None,
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
                                    egui::Color32::YELLOW,
                                    format!("⏳ 等待定时开始（{}）", format_left(p.waiting_left())),
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

        egui::CentralPanel::default().show(ui, |ui| {
            match self.tab {
                AppTab::Send => self.ui_send(ui),
                AppTab::Settings => self.ui_settings(ui),
            }

            ui.add_space(4.0);
            self.ui_logs(ui);
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(200));
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
                        ui.add(
                            egui::TextEdit::singleline(&mut self.to)
                                .hint_text("多个收件人用英文逗号分隔")
                                .desired_width(320.0),
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
                ui.horizontal(|ui| {
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
                ui.checkbox(&mut self.sched_enabled, "⏰ 定时开始（仅本次发送有效）");

                ui.add_enabled_ui(self.sched_enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("开始时间");
                        ui.add(
                            egui::DragValue::new(&mut self.sched_date.0)
                                .range(2024..=2100)
                                .suffix(" 年"),
                        );
                        ui.add(egui::DragValue::new(&mut self.sched_date.1).range(1..=12).suffix(" 月"));
                        ui.add(egui::DragValue::new(&mut self.sched_date.2).range(1..=31).suffix(" 日"));
                        ui.add(egui::DragValue::new(&mut self.sched_time.0).range(0..=23).suffix(" 时"));
                        ui.add(egui::DragValue::new(&mut self.sched_time.1).range(0..=59).suffix(" 分"));
                        ui.add(egui::DragValue::new(&mut self.sched_time.2).range(0..=59).suffix(" 秒"));
                    });
                    ui.horizontal(|ui| {
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
                            let left = (t - chrono::Local::now().naive_local()).num_seconds();
                            if left > 0 {
                                ui.label(format!(
                                    "将于 {} 开始（剩余 {}）",
                                    t.format("%Y-%m-%d %H:%M:%S"),
                                    format_left(left as u64)
                                ));
                            } else {
                                ui.colored_label(
                                    egui::Color32::YELLOW,
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

    /// 把控件时间整体平移（快捷按钮用）
    fn shift_schedule(&mut self, delta: Duration) {
        let base = self
            .schedule_target()
            .unwrap_or_else(|_| chrono::Local::now().naive_local());
        let t = base + delta;
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
                egui::Color32::YELLOW,
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
                    let left = (t - chrono::Local::now().naive_local()).num_seconds();
                    if left > 0 {
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
                    self.config.save();
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
                    self.edit_index = idx;
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
                            self.config.save();
                        }
                        self.edit_index = None;
                        self.edit_account_id = None;
                    }
                    AccountEditAction::Cancel => {
                        self.edit_account = None;
                        self.edit_index = None;
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
                self.config.save();
                self.logger.info("配置已保存");
            }
        });
    }

    /* =================== 日志区 =================== */

    fn ui_logs(&self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("运行日志")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for log in self.logger.entries() {
                            let color = match log.level {
                                LogLevel::Info => egui::Color32::LIGHT_GREEN,
                                LogLevel::Warn => egui::Color32::YELLOW,
                                LogLevel::Error => egui::Color32::RED,
                            };
                            ui.colored_label(color, &log.message);
                        }
                    });
            });
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
