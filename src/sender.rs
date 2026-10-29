use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lettre::{
    message::{
        header::ContentType,
        MultiPart, SinglePart,
    },
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};

use crate::config::{AppConfig, SmtpAccount};
use crate::logger::Logger;
use crate::progress::SendProgress;
use crate::sevenz::{split_if_needed, cleanup_parts};

/// 发送主入口（由 app.rs 在后台线程中调用）
pub fn start_send_task(
    files: &[PathBuf],
    to: &str,
    subject: &str,
    body: &str,
    config: &AppConfig,
    progress: Arc<SendProgress>,
    logger: Logger,
) {
    logger.info("开始发送任务");

    let mut account_index = 0usize;

    for (file_idx, file) in files.iter().enumerate() {
        if progress.is_stopped() {
            logger.warn("发送被用户终止");
            break;
        }

        progress.set_current_file((file_idx + 1) as u64);

        // === 分卷 / 原文件 ===
        let work_dir = temp_work_dir();
        let result = match split_if_needed(file, &work_dir, config) {
            Ok(r) => r,
            Err(e) => {
                logger.error(format!(
                    "文件分卷失败 {}: {}",
                    file.display(),
                    e
                ));
                continue;
            }
        };

        progress.reset_file_parts(result.parts.len() as u64);

        for part in &result.parts {
            if progress.is_stopped() {
                break;
            }

            progress.wait_if_paused();

            // === 找可用账号 ===
            let mut sent = false;
            let mut tried = 0usize;

            while !sent && tried < config.accounts.len() {
                let acc = &config.accounts[account_index];
                logger.info(format!("使用账号 [{}] 发送 {}", acc.name, part.display()));

                match send_with_account(acc, to, subject, body, part) {
                    Ok(_) => {
                        sent = true;
                        logger.info(format!("发送成功: {}", part.display()));
                    }
                    Err(e) => {
                        logger.error(format!(
                            "账号 [{}] 发送失败: {}",
                            acc.name, e
                        ));
                        account_index = (account_index + 1) % config.accounts.len();
                        tried += 1;
                    }
                }
            }

            if !sent {
                logger.error("所有 SMTP 账号均不可用，终止发送");
                progress.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }

            progress.inc_sent_part();

            // 发送间隔
            thread::sleep(Duration::from_secs(config.interval_sec));
        }

        cleanup_parts(&result.parts, file);
    }
    logger.info("发送任务结束");
}

/// 使用单个账号发送一个附件
fn send_with_account(
    acc: &SmtpAccount,
    to: &str,
    subject: &str,
    body: &str,
    attachment: &Path,
) -> Result<(), String> {
    let mailer = build_mailer(acc)?;

    let mut builder = Message::builder()
        .from(acc.username.parse::<lettre::message::Mailbox>().map_err(|e| e.to_string())?)
        .subject(subject);

    for addr in to.split(',') {
        builder = builder.to(addr.trim().parse::<lettre::message::Mailbox>().map_err(|e| e.to_string())?);
    }

    let bytes = std::fs::read(attachment).map_err(|e| e.to_string())?;
    let filename = attachment
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.bin")
        .to_string();

    let attach_part = SinglePart::builder()
        .header(ContentType::parse("application/octet-stream").unwrap())
        .header(lettre::message::header::ContentDisposition::attachment(filename.as_str()))
        .body(bytes);

    let email = builder
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body.to_string()))
                .singlepart(attach_part),
        )
        .map_err(|e| e.to_string())?;

    mailer.send(&email).map_err(|e| e.to_string())?;
    Ok(())
}

/// 构建 SMTP 客户端
fn build_mailer(acc: &SmtpAccount) -> Result<SmtpTransport, String> {
    let creds = Credentials::new(
        acc.username.clone(),
        acc.password.clone(),
    );

    let mailer = if acc.ssl {
        // SMTPS（465）
        SmtpTransport::relay(&acc.smtp)
            .map_err(|e| e.to_string())?
            .port(acc.port)
            .credentials(creds)
            .build()
    } else {
        // STARTTLS / 明文
        SmtpTransport::builder_dangerous(&acc.smtp)
            .port(acc.port)
            .credentials(creds)
            .build()
    };

    Ok(mailer)
}

/// 临时工作目录（exe 同目录 / temp）
fn temp_work_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("rust_mail_sender");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
