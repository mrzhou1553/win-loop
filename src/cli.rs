use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

#[cfg(windows)]
mod applications;
#[cfg(windows)]
mod recorder;
mod wav;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("录音失败：{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let Some(path) = output_path()? else {
        return Ok(());
    };
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || signal.store(false, Ordering::Relaxed))
        .context("无法注册 Ctrl+C 处理器")?;

    #[cfg(windows)]
    {
        wasapi::initialize_mta().ok().context("无法初始化 COM")?;
        let result = (|| -> Result<()> {
            if let Some(target) = applications::select_target(&running)?
                && running.load(Ordering::Relaxed)
            {
                match target {
                    applications::RecordingTarget::All => recorder::record(&path, &running)?,
                    applications::RecordingTarget::Process(target) => {
                        recorder::record_process(&path, target.pid, &target.name, &running)?;
                    }
                }
            }
            Ok(())
        })();
        // 录音和应用枚举返回后，它们持有的 COM 接口已经全部释放。
        wasapi::deinitialize();
        result
    }
    #[cfg(not(windows))]
    {
        let _ = (path, running);
        bail!("本程序仅支持 Windows");
    }
}

fn output_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if matches!(first.as_deref(), Some(value) if value == "--help" || value == "-h") {
        println!(
            "用法：win-loop-cli [输出.wav]\n启动后列出有音频会话的应用，输入 PID 录制该进程及其子进程。\n输入 ALL 录制默认输出设备的全部声音（大小写均可）。\n输入 r 刷新列表，q 退出；录音时按 Ctrl+C 保存并退出。\n不指定路径时保存到 recordings 目录。已有文件不会被覆盖。"
        );
        return Ok(None);
    }
    if args.next().is_some() {
        bail!("只支持一个输出路径参数；使用 --help 查看用法");
    }
    let path = match first {
        Some(value) => PathBuf::from(value),
        None => {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
            PathBuf::from(format!("recordings/recording-{timestamp}.wav"))
        }
    };
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
    {
        bail!("输出文件必须使用 .wav 扩展名");
    }
    Ok(Some(path))
}
