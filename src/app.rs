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
        Self {
            tab: AppTab::Send,
            config: AppConfig::load(),
            logger: Logger::new(100),
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
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("📧 MailSender");
                if self.sending {
                    ui.label("发送中…");
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
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
            egui::ComboBox::from_id_source("recipient_history")
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

        ui.horizontal(|ui| {
            if ui.button("⏸ 暂停").clicked() {
                progress.paused.store(true, Ordering::Relaxed);
            }
            if ui.button("▶ 继续").clicked() {
                progress.paused.store(false, Ordering::Relaxed);
            }
            if ui.button("⛔ 停止").clicked() {
                // self.send_state = SendState::Stopping;
                self.sending = false;
                progress.stopped.store(true, Ordering::Relaxed);
            }
        });
    }

    fn start_send(&mut self) {
        if self.files.is_empty() || self.config.accounts.is_empty() {
            self.logger.error("文件或 SMTP 账号为空，无法发送");
            return;
        }

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
                }
                AccountEditAction::Cancel => {
                    self.edit_account = None;
                    self.edit_index = None;
                }
            }
        }

        // ===== 全局参数 =====
        ui.separator();
        ui.heading("全局参数");

        ui.horizontal(|ui| {
            ui.label("邮件发送间隔（秒）:");
            ui.add(egui::DragValue::new(&mut self.config.interval_sec).suffix(" 秒"));
        });

        ui.horizontal(|ui| {
            ui.label("7z 分卷大小（MB）:");
            ui.add(egui::DragValue::new(&mut self.config.split_mb).suffix(" MB"));
        });

        ui.horizontal(|ui| {
            ui.label("7z 密码:");
            ui.text_edit_singleline(&mut self.config.sevenz_password);
        });

        if ui.button("💾 保存配置").clicked() {
            self.config.save();
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
