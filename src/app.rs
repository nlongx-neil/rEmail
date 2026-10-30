use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use eframe::egui;
use uuid::Uuid;

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

    /// 正文
    body: String,

    /// 选择的文件
    files: Vec<PathBuf>,

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

        Self {
            tab: AppTab::Send,
            config,
            logger,
            progress: None,
            sending: false,
            to: String::new(),
            subject: String::new(),
            body: String::new(),
            files: Vec::new(),
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
            if let Some(p) = &self.progress {
                if p.is_finished() {
                    self.sending = false;
                }
            }
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("📧 MailSender");
                if self.sending {
                    if let Some(p) = &self.progress {
                        if p.is_waiting() {
                            ui.label(format!("⏳ 等待定时开始（剩余 {} 秒）", p.waiting_left()));
                        } else if p.is_stopped() {
                            ui.label("正在停止…");
                        } else {
                            ui.label("发送中…");
                        }
                    } else {
                        ui.label("发送中…");
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // ===== Tabs =====
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.tab == AppTab::Send, "发送")
                    .clicked()
                {
                    self.tab = AppTab::Send;
                }
                if ui
                    .selectable_label(self.tab == AppTab::Settings, "设置")
                    .clicked()
                {
                    self.tab = AppTab::Settings;
                }
            });
            ui.separator();

            match self.tab {
                AppTab::Send => self.ui_send(ui),
                AppTab::Settings => self.ui_settings(ui),
            }

            ui.separator();
            self.ui_logs(ui);
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

impl MailApp {
    /* =================== 发送页 =================== */
    fn ui_send(&mut self, ui: &mut egui::Ui) {
        ui.label("收件人（多个用逗号分隔）");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.to);
            egui::ComboBox::from_id_salt("recipient_history")
                .width(200.0)
                .selected_text("历史")
                .show_ui(ui, |ui| {
                    for email in &self.config.history.recipients {
                        if ui.selectable_label(false, email).clicked() {
                            self.to = email.clone();
                        }
                    }
                });
        });

        ui.label("主题");
        ui.text_edit_singleline(&mut self.subject);

        ui.label("正文");
        ui.text_edit_multiline(&mut self.body);

        ui.separator();

        if ui.button("选择文件").clicked() {
            if let Some(files) = rfd::FileDialog::new().pick_files() {
                self.files = files;
            }
        }

        for f in &self.files {
            ui.label(f.display().to_string());
        }

        ui.separator();

        if !self.sending {
            if ui.button("🚀 开始发送").clicked() {
                self.start_send();
            }
        } else if let Some(p) = &self.progress {
            self.ui_progress(ui, p.clone());
        }
    }

    fn ui_progress(&mut self, ui: &mut egui::Ui, progress: Arc<SendProgress>) {
        // 定时等待阶段：进度条还没意义，展示倒计时
        if progress.is_waiting() {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("⏳ 等待定时开始，剩余 {} 秒", progress.waiting_left()),
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
                // 只置停止标记，等后台线程真正结束后由 update() 复位 sending
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

        // 定时开始时间是否合法（启用时）
        if let Err(e) = self.config.parse_schedule_start() {
            self.logger.error(format!("定时开始配置无效: {}", e));
            return;
        }

        // 发送前统一修正配置，避免 min > max 等异常区间
        for note in self.config.normalize() {
            self.logger.warn(format!("配置已修正: {}", note));
        }

        let (min_sec, max_sec) = self.config.interval_range();
        self.logger.info(format!(
            "本次发送随机间隔区间: {} ~ {} 秒",
            min_sec, max_sec
        ));

        let progress = Arc::new(SendProgress::new(self.files.len() as u64));
        let logger = self.logger.clone();
        let config = self.config.clone();
        let files = self.files.clone();
        let to = self.to.clone();
        let subject = self.subject.clone();
        let body = self.body.clone();

        self.progress = Some(progress.clone());
        self.sending = true;
        self.config.history.record(&to);
        thread::spawn(move || {
            start_send_task(&files, &to, &subject, &body, &config, progress, logger);
        });
    }

    /* =================== 设置页 =================== */

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("SMTP 账号");

        let mut remove_index: Option<usize> = None;
        let mut start_edit: Option<(Option<usize>, SmtpAccount)> = None;

        // ===== 账号列表 =====
        for (i, acc) in self.config.accounts.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&acc.name);

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

        // ===== 编辑面板 =====
        let mut action: Option<AccountEditAction> = None;

        if let Some(acc) = &mut self.edit_account {
            ui.separator();
            ui.heading("账号配置");

            ui.horizontal(|ui| {
                ui.label("账号名称:");
                ui.text_edit_singleline(&mut acc.name);
            });

            ui.horizontal(|ui| {
                ui.label("SMTP地址:");
                ui.text_edit_singleline(&mut acc.smtp);
            });

            ui.horizontal(|ui| {
                ui.label("SMTP端口:");
                ui.add(egui::DragValue::new(&mut acc.port));
            });

            ui.horizontal(|ui| {
                ui.label("账号:");
                ui.text_edit_singleline(&mut acc.username);
            });
            ui.horizontal(|ui| {
                ui.label("密码:");
                ui.text_edit_singleline(&mut acc.password);
            });
            ui.horizontal(|ui| {
                ui.label("开启SSL:");
                ui.checkbox(&mut acc.ssl, "SSL");
            });

            ui.horizontal(|ui| {
                if ui.button("💾 保存").clicked() {
                    action = Some(AccountEditAction::Save);
                }
                if ui.button("取消").clicked() {
                    action = Some(AccountEditAction::Cancel);
                }
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

        // ===== 全局参数 =====
        ui.separator();
        ui.heading("全局参数");

        ui.horizontal(|ui| {
            ui.label("邮件发送随机间隔（秒）:");
            // 未配置时以旧的 interval_sec 作为可编辑初值
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
        });
        if self
            .config
            .interval_min_sec
            .zip(self.config.interval_max_sec)
            .is_some_and(|(min, max)| min > max)
        {
            ui.colored_label(
                egui::Color32::RED,
                "最小值大于最大值，保存时会自动交换",
            );
        }
        ui.label(format!(
            "实际间隔区间: {} ~ {} 秒",
            self.config.interval_range().0,
            self.config.interval_range().1
        ));

        ui.horizontal(|ui| {
            ui.label("7z 分卷大小（MB）:");
            ui.add(
                egui::DragValue::new(&mut self.config.split_mb)
                    .range(1..=10_000u64)
                    .suffix(" MB"),
            );
        });

        ui.horizontal(|ui| {
            ui.label("7z 密码:");
            ui.text_edit_singleline(&mut self.config.sevenz_password);
        });

        ui.separator();
        ui.heading("定时开始");
        ui.checkbox(&mut self.config.schedule_enabled, "启用定时开始发送");

        ui.horizontal(|ui| {
            ui.label("开始时间:");
            ui.add(
                egui::TextEdit::singleline(&mut self.config.schedule_start)
                    .hint_text("YYYY-MM-DD HH:MM:SS")
                    .desired_width(200.0),
            );
            if ui.button("设为 1 分钟后").clicked() {
                let t = chrono::Local::now().naive_local()
                    + chrono::Duration::minutes(1);
                self.config.schedule_start = t.format("%Y-%m-%d %H:%M:%S").to_string();
            }
        });

        if self.config.schedule_enabled {
            match self.config.parse_schedule_start() {
                Ok(Some(t)) => {
                    let left = (t - chrono::Local::now().naive_local()).num_seconds();
                    if left > 0 {
                        ui.label(format!("将在 {} 开始（约 {} 秒后）", t, left));
                    } else {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "该时间已过，点击开始后会立即发送",
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    ui.colored_label(egui::Color32::RED, e);
                }
            }
        }

        if ui.button("💾 保存配置").clicked() {
            for note in self.config.normalize() {
                self.logger.warn(format!("配置已修正: {}", note));
            }
            self.config.save();
            self.logger.info("配置已保存");
        }
    }

    /* =================== 日志页 =================== */

    fn ui_logs(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            for log in self.logger.entries() {
                let color = match log.level {
                    LogLevel::Info => egui::Color32::LIGHT_GREEN,
                    LogLevel::Warn => egui::Color32::YELLOW,
                    LogLevel::Error => egui::Color32::RED,
                };
                ui.colored_label(color, &log.message);
            }
        });
    }
}
