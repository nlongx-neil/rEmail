use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let target = env::var("TARGET").unwrap_or_default();
    let is_windows = target.contains("windows");

    // Windows：把 assets/icon.ico 作为资源嵌入 exe（任务栏 / 资源管理器显示自定义图标）
    if is_windows {
        embed_windows_icon();
    }

    // 仅发布构建 + Windows 时用 UPX 压缩（UPX 只处理 PE，其他平台跳过）
    if env::var("PROFILE").unwrap_or_default() == "release" {
        if !is_windows {
            println!("cargo:warning=非 Windows 目标，跳过 UPX 压缩");
            return;
        }
        compress_with_upx(&target);
    }
}

fn embed_windows_icon() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "MailSender");
        res.set("ProductName", "MailSender");

        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入 exe 图标失败（不影响编译）: {e}");
        }
    }

    // 在非 Windows 主机上交叉编译到 Windows 时无法调用 windres，
    // 需要在 Windows 上构建，或构建后用 rcedit 手动写入图标。
    #[cfg(not(windows))]
    println!("cargo:warning=当前构建主机非 Windows，跳过 exe 图标嵌入（请在 Windows 上构建或使用 rcedit）");
}

fn compress_with_upx(target: &str) {
    let exe_name = if target.contains("windows") {
        "mail_sender.exe"
    } else {
        "mail_sender"
    };

    // OUT_DIR = <target-dir>/<profile>/build/<pkg>-<hash>/out
    let out_dir = match env::var("OUT_DIR") {
        Ok(v) => v,
        Err(_) => return,
    };
    let target_dir = Path::new(&out_dir)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());

    let exe_path = match target_dir {
        Some(dir) => dir.join(exe_name),
        None => return,
    };

    println!("cargo:warning=正在检查可执行文件: {}", exe_path.display());

    if !exe_path.exists() {
        println!("cargo:warning=未找到可执行文件: {}", exe_path.display());
        return;
    }

    println!("cargo:warning=正在使用UPX压缩可执行文件...");
    let status = Command::new("upx")
        .arg("--best")
        .arg("--lzma")
        .arg(&exe_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:warning=UPX压缩成功");
            let _ = Command::new("upx").arg("-l").arg(&exe_path).status();
        }
        Ok(s) => println!("cargo:warning=UPX压缩失败，退出码: {}", s),
        Err(e) => {
            println!("cargo:warning=UPX未安装或无法运行: {}", e);
            println!("cargo:warning=请安装UPX: https://upx.github.io/");
        }
    }
}
