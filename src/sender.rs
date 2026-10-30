use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
///
/// 该函数整体运行在后台线程，因此定时等待也不会阻塞 UI 线程。
/// `schedule_at` 是本次发送的一次性定时开始时间（None 表示立即发送），
/// 不读取 config 里的全局定时字段。
pub fn start_send_task(
    files: &[PathBuf],
    to: &str,
    subject: &str,
    body: &str,
    config: &AppConfig,
    schedule_at: Option<chrono::NaiveDateTime>,
    progress: Arc<SendProgress>,
    logger: Logger,
) {
    logger.info("开始发送任务");

    // 前置校验：避免后续出现除零 / 空账号下标越界
    if files.is_empty() {
        logger.error("没有选择要发送的文件，任务取消");
        progress.mark_finished();
        return;
    }
    if config.accounts.is_empty() {
        logger.error("没有可用的 SMTP 账号，任务取消");
        progress.mark_finished();
        return;
    }

    // 收件人格式先校验一次，避免每个分卷都重复失败
    if let Err(e) = validate_recipients(to) {
        logger.error(format!("收件人格式错误: {}", e));
        progress.mark_finished();
        return;
    }

    // 定时开始：在后台线程内等待，UI / 日志线程不受影响
    if !wait_until_start(schedule_at, &progress, &logger) {
        logger.warn("定时等待被终止或配置非法，任务结束");
        progress.mark_finished();
        return;
    }

    let (min_sec, max_sec) = config.interval_range();
    if max_sec > 0 {
        logger.info(format!(
            "每封邮件随机间隔: {}-{} 秒",
            min_sec, max_sec
        ));
    } else {
        logger.warn("发送间隔为 0 秒，将连续发送");
    }

    if config.sevenz_password.is_empty() {
        logger.warn("7z 密码为空：需要分卷的附件将不加密（文件名也不会加密）");
    }

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

        for (part_idx, part) in result.parts.iter().enumerate() {
            if progress.is_stopped() {
                break;
            }

            progress.wait_if_paused();
            if progress.is_stopped() {
                break;
            }

            // === 找可用账号 ===
            let mut sent = false;
            let mut tried = 0usize;

            while !sent && tried < config.accounts.len() && !progress.is_stopped() {
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
                if progress.is_stopped() {
                    logger.warn("发送被用户终止");
                } else {
                    logger.error("所有 SMTP 账号均不可用，终止发送");
                    progress
                        .stopped
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                break;
            }

            progress.inc_sent_part();

            // 发送间隔：最后一封邮件之后无需再等待
            let is_last_part =
                file_idx + 1 == files.len() && part_idx + 1 == result.parts.len();
            if !is_last_part && (min_sec > 0 || max_sec > 0) {
                let wait = random_interval_sec(min_sec, max_sec);
                logger.info(format!("等待 {} 秒后发送下一封（随机间隔）", wait));
                sleep_interruptible(&progress, wait);
            }
        }

        cleanup_parts(&result.parts, file);
    }

    if progress.is_stopped() {
        logger.warn("发送任务已终止");
    } else {
        logger.info("发送任务结束");
    }
    progress.mark_finished();
}

/// 等待定时开始时间；返回 false 表示被终止
fn wait_until_start(
    schedule_at: Option<chrono::NaiveDateTime>,
    progress: &SendProgress,
    logger: &Logger,
) -> bool {
    let target = match schedule_at {
        Some(t) => t,
        None => return true,
    };

    let fmt = "%Y-%m-%d %H:%M:%S";
    let now = chrono::Local::now().naive_local();
    // 直接比较时间点：num_seconds() 会截断不到 1 秒的差值，导致提前 <=1 秒开始
    if now >= target {
        logger.info(format!(
            "开始时间 {} 已到/已过，立即开始发送",
            target.format(fmt)
        ));
        return true;
    }
    let initial_left = (target - now).num_seconds();

    logger.info(format!(
        "已启用定时开始，等待至 {}（剩余 {} 秒），期间可点击「取消等待」终止本次发送",
        target.format(fmt),
        initial_left
    ));

    let mut last_logged: Option<i64> = Some(initial_left);
    loop {
        if progress.is_stopped() {
            progress.clear_waiting();
            logger.warn("等待定时开始期间被用户终止");
            return false;
        }

        let now = chrono::Local::now().naive_local();
        if now >= target {
            progress.clear_waiting();
            logger.info(format!("已到达定时开始时间 {}，开始发送", target.format(fmt)));
            return true;
        }
        let left = (target - now).num_seconds();

        progress.set_waiting(left as u64);

        // 每跨过一整分钟打印一次剩余时间，避免日志刷屏
        if last_logged.map_or(true, |l| l > left && l - left >= 60) {
            logger.info(format!("等待定时开始，剩余 {} 秒", left));
            last_logged = Some(left);
        }

        // 分片睡眠，保证「停止」能在一个周期内生效
        thread::sleep(Duration::from_millis(500));
    }
}

/// 在 [min, max] 内取随机秒数（无新增依赖）
fn random_interval_sec(min: u64, max: u64) -> u64 {
    if max <= min {
        return min;
    }

    let mut hasher = RandomState::new().build_hasher();
    // RandomState 由系统随机种子初始化；再混入纳秒时间，进一步降低可预测性
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    hasher.write_u64(nanos);
    let r = hasher.finish();

    min + r % (max - min + 1)
}

/// 可被「停止 / 暂停」打断的 sleep
fn sleep_interruptible(progress: &SendProgress, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);

    while !progress.is_stopped() {
        progress.wait_if_paused();
        if progress.is_stopped() {
            break;
        }

        let now = Instant::now();
        let remain = deadline.saturating_duration_since(now);
        if remain.is_zero() {
            break;
        }
        thread::sleep(remain.min(Duration::from_millis(500)));
    }
}

/// 校验收件人列表（逗号分隔）
fn validate_recipients(to: &str) -> Result<(), String> {
    let mut any = false;
    for addr in to.split(',') {
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }
        addr.parse::<lettre::message::Mailbox>()
            .map_err(|e| format!("「{}」无效: {}", addr, e))?;
        any = true;
    }

    if any {
        Ok(())
    } else {
        Err("收件人为空".to_string())
    }
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

    let content_type =
        ContentType::parse("application/octet-stream").map_err(|e| e.to_string())?;

    let attach_part = SinglePart::builder()
        .header(content_type)
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

/// 临时工作目录（系统 temp/rust_mail_sender）
fn temp_work_dir() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push("rust_mail_sender");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn random_interval_in_range() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let v = random_interval_sec(1, 3);
            assert!((1..=3).contains(&v), "越界: {}", v);
            seen.insert(v);
        }
        // 证明取值不是常量（确实在随机）
        assert!(seen.len() >= 2, "随机间隔未体现随机性: {:?}", seen);
    }

    #[test]
    fn random_interval_equal_bounds() {
        assert_eq!(random_interval_sec(5, 5), 5);
        assert_eq!(random_interval_sec(5, 4), 5);
    }

    #[test]
    fn recipients_validation() {
        assert!(validate_recipients("a@b.com, c@d.com").is_ok());
        assert!(validate_recipients("   ").is_err());
        assert!(validate_recipients("not-an-email").is_err());
    }

    /// 定时开始：设置 ~3 秒后开始，验证确实会等待到点
    #[test]
    fn wait_until_start_sleeps_until_target() {
        let target =
            chrono::Local::now().naive_local() + chrono::Duration::seconds(3);

        let progress = SendProgress::new(1);
        let logger = Logger::new(10);

        let started = Instant::now();
        assert!(wait_until_start(Some(target), &progress, &logger));
        let elapsed = started.elapsed().as_secs_f64();

        assert!(
            (1.5..=5.0).contains(&elapsed),
            "实际等待 {elapsed}s，与预期 2~4s 不符"
        );
        assert!(!progress.is_waiting(), "到点后应退出等待状态");
    }

    /// 定时等待期间点击「停止」应立即返回，不需要等到目标时间
    #[test]
    fn wait_until_start_aborts_when_stopped() {
        let target =
            chrono::Local::now().naive_local() + chrono::Duration::hours(1);

        let progress = SendProgress::new(1);
        progress
            .stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let logger = Logger::new(10);

        let started = Instant::now();
        assert!(!wait_until_start(Some(target), &progress, &logger));
        assert!(started.elapsed().as_secs() < 2, "停止未能及时生效");
        assert!(!progress.is_waiting());
    }

    /// 无定时：立即返回
    #[test]
    fn wait_until_start_none_returns_immediately() {
        let progress = SendProgress::new(1);
        let logger = Logger::new(10);
        let started = Instant::now();
        assert!(wait_until_start(None, &progress, &logger));
        assert!(started.elapsed().as_millis() < 500);
    }

    /// 可中断间隔等待：到点前被停止则提前返回
    #[test]
    fn interruptible_sleep_respects_stop() {
        let progress = SendProgress::new(1);
        let progress2 = progress.clone();
        std::thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            progress2
                .stopped
                .store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let started = Instant::now();
        sleep_interruptible(&progress, 5);
        let elapsed = started.elapsed().as_secs_f64();
        assert!(elapsed < 2.0, "停止后仍等待了 {elapsed}s");
    }
}
