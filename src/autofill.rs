//! 根据附件自动生成主题 / 正文的纯函数逻辑（无 UI 依赖，便于单测）

use std::path::PathBuf;

/// 附件汇总信息
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentSummary {
    /// 文件个数
    pub count: usize,
    /// 总字节数（读不到元数据的文件按 0 计）
    pub total_bytes: u64,
    /// 第一个附件的文件名主干（不含扩展名）
    pub first_stem: String,
}

/// 汇总附件信息；无附件时返回 None
pub fn summarize_attachments(files: &[PathBuf]) -> Option<AttachmentSummary> {
    let first = files.first()?;
    let first_stem = first
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .or_else(|| first.file_name().and_then(|s| s.to_str()))
        .unwrap_or("附件")
        .to_string();

    let total_bytes = files
        .iter()
        .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
        .sum();

    Some(AttachmentSummary {
        count: files.len(),
        total_bytes,
        first_stem,
    })
}

/// 生成主题：单附件取文件名主干；多附件为「主干 等 N 个文件」；无附件返回 None
pub fn suggest_subject(files: &[PathBuf]) -> Option<String> {
    let sum = summarize_attachments(files)?;
    if sum.count == 1 {
        Some(sum.first_stem)
    } else {
        Some(format!("{} 等 {} 个文件", sum.first_stem, sum.count))
    }
}

/// 生成正文（正式、简短）：包含文件数量、总体积、分卷与加密提示
pub fn suggest_body(files: &[PathBuf], split_mb: u64, encrypted: bool) -> Option<String> {
    let sum = summarize_attachments(files)?;
    let enc = if encrypted {
        "，免密另行告知"
    } else {
        ""
    };
    Some(format!(
        "您好：\n\n随邮件发送 {n} 个文件，总大小 {size}。\
         附件已按每卷 {split} MB 分卷压缩{enc}，请收齐全部分卷后解压。\n\n\
         此致\n敬礼",
        n = sum.count,
        size = format_size(sum.total_bytes),
        split = split_mb,
        enc = enc,
    ))
}

/// 人类可读体积（B/KB/MB/GB，保留 1 位小数）
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// 字段更新决策：用户手动编辑过（dirty）就不覆盖；否则采用生成值，
/// 生成值为 None（无附件）时清空——调用方保证 dirty=false 时当前内容就是程序生成的。
pub fn next_field_value(generated: Option<&str>, user_dirty: bool) -> Option<String> {
    if user_dirty {
        None // 保留用户内容
    } else {
        Some(generated.unwrap_or("").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_file(name: &str, bytes: usize) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mailsender_autofill_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
        p
    }

    #[test]
    fn subject_single_attachment_uses_stem() {
        let f = tmp_file("季度报告.pdf", 10);
        assert_eq!(suggest_subject(&[f]).as_deref(), Some("季度报告"));
    }

    #[test]
    fn subject_multiple_attachments() {
        let f1 = tmp_file("a.docx", 1);
        let f2 = tmp_file("b.xlsx", 1);
        let f3 = tmp_file("c.png", 1);
        assert_eq!(
            suggest_subject(&[f1, f2, f3]).as_deref(),
            Some("a 等 3 个文件")
        );
    }

    #[test]
    fn no_attachment_returns_none() {
        assert_eq!(suggest_subject(&[]), None);
        assert_eq!(suggest_body(&[], 10, true), None);
        assert_eq!(summarize_attachments(&[]), None);
    }

    #[test]
    fn body_contains_count_size_split_and_encryption() {
        let f1 = tmp_file("x.bin", 5 * 1024 * 1024);
        let f2 = tmp_file("y.bin", 5 * 1024 * 1024);
        let body = suggest_body(&[f1, f2], 10, true).unwrap();
        assert!(body.contains("2 个文件"), "{body}");
        assert!(body.contains("10.0 MB"), "{body}");
        assert!(body.contains("每卷 10 MB"), "{body}");
        assert!(body.contains("已加密"), "{body}");

        let f = tmp_file("z.bin", 100);
        let plain = suggest_body(&[f], 20, false).unwrap();
        assert!(!plain.contains("已加密"), "{plain}");
        assert!(plain.contains("100 B"), "{plain}");
    }

    #[test]
    fn dirty_field_is_never_overwritten() {
        // 用户已编辑：即使有新的生成值也不覆盖
        assert_eq!(next_field_value(Some("生成的"), true), None);
        // 用户已编辑：无附件时也不清空
        assert_eq!(next_field_value(None, true), None);
    }

    #[test]
    fn clean_field_follows_generated_value() {
        assert_eq!(
            next_field_value(Some("生成的"), false).as_deref(),
            Some("生成的")
        );
        // 无附件时清空程序生成内容
        assert_eq!(next_field_value(None, false).as_deref(), Some(""));
    }
}
