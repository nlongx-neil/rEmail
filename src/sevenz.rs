use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use std::io;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::config::AppConfig;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
/// 7z 分卷结果
#[derive(Debug)]
pub struct SevenZResult {
    /// 分卷文件列表（按顺序）
    pub parts: Vec<PathBuf>,
}

/// 使用 7z 进行分卷压缩
///
/// - 输入：原始文件
/// - 输出：
///   xxx.7z.001
///   xxx.7z.002
///   ...
///
/// 规则：
/// - 如果文件 <= split_mb，返回单文件（原文件）
/// - 如果 > split_mb，生成分卷并返回分卷路径
pub fn split_if_needed(
    input: &Path,
    work_dir: &Path,
    config: &AppConfig,
) -> io::Result<SevenZResult> {
    let meta = fs::metadata(input)?;
    let split_mb = config.split_mb.max(1);
    // 用字节比较而不是「截断成 MB」，否则 10.9MB 文件会被误判为不需要分卷

    // 不需要分卷
    if !needs_split(meta.len(), split_mb) {
        return Ok(SevenZResult {
            parts: vec![input.to_path_buf()],
        });
    }

    // 输出文件名
    let base_name = input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "非法文件名"))?;

    let output_7z = work_dir.join(format!("{}.7z", base_name));
    let prefix = format!("{}.7z.", base_name);
    // 先清掉上次中断残留的同名分卷，避免本次生成 3 个分卷却把旧的 004/005 一起发出去
    remove_stale_parts(work_dir, &prefix);

    // 构造 7z 命令
    let mut cmd = Command::new(sevenz_path()?);
    cmd.arg("a")
        .arg(&output_7z)
        .arg(input)
        .arg(format!("-v{}m", split_mb))
        .arg("-y");
    // 密码为空时不能传 "-p"（7z 会进入交互式输入），此时按不加密处理
    if !config.sevenz_password.is_empty() {
        cmd.arg(format!("-p{}", config.sevenz_password))
            .arg("-mhe=on"); // 文件名加密
    }
    hide_console(&mut cmd);

    let output = cmd.output()?;

    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "7z 压缩失败: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    // 查找生成的分卷文件
    let mut parts = Vec::new();

    for entry in fs::read_dir(work_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name.starts_with(&prefix) {
            parts.push(entry.path());
        }
    }

    // 按扩展名数字排序（.001/.002/…），避免字典型排序把 .010 排到 .002 前面
    parts.sort_by_key(|p| {
        p.extension()
            .and_then(|e| e.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "未找到 7z 分卷输出",
        ));
    }

    Ok(SevenZResult { parts })
}

/// 是否需要分卷：按字节精确比较（<= 阈值则直接发原文件）
fn needs_split(size_bytes: u64, split_mb: u64) -> bool {
    size_bytes > split_mb.max(1).saturating_mul(1024 * 1024)
}

/// 删除工作目录里上一次残留的同名分卷（发送前清理，避免旧分卷混入）
fn remove_stale_parts(work_dir: &Path, prefix: &str) {
    if let Ok(entries) = fs::read_dir(work_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// 隐藏子进程控制台窗口（仅 Windows 需要）
fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// 获取 7z 可执行文件路径
///
/// 策略（跨平台）：
/// 1️⃣ 优先使用 exe 同目录下的 7z（Windows: 7z.exe；macOS/Linux: 7z / 7zz / 7za）
/// 2️⃣ 其次在 PATH 中查找
/// 3️⃣ 都没有时回落到默认命令名，交由 Command 报错并提示安装
fn sevenz_path() -> io::Result<PathBuf> {
    // Homebrew 的 sevenzip 提供 7zz，p7zip 提供 7z / 7za
    let names: &[&str] = if cfg!(windows) {
        &["7z.exe", "7z"]
    } else {
        &["7z", "7zz", "7za"]
    };

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    {
        for name in names {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    if let Some(path) = find_in_path(names) {
        return Ok(path);
    }

    Ok(PathBuf::from(names[0]))
}

/// 在 PATH 中按顺序查找可执行文件
fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in names {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 清理分卷文件（发送完成后）
pub fn cleanup_parts(parts: &[PathBuf], original: &Path) {
    for p in parts {
        if p != original {
            let _ = fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_threshold_uses_exact_bytes() {
        let mb = 1024 * 1024;
        // 恰好等于阈值：不分卷
        assert!(!needs_split(10 * mb, 10));
        // 10.9MB：原实现按 MB 截断成 10MB 会误判为不分卷
        assert!(needs_split(10 * mb + mb * 9 / 10, 10));
        // 0 / 非法阈值按 1MB 处理
        assert!(needs_split(mb + 1, 0));
    }
}
