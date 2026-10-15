#![cfg_attr(windows, windows_subsystem = "windows")]

// 共享模块中的终端交互入口仍供 win-loop-cli 使用。
#[cfg(windows)]
#[allow(dead_code)]
mod applications;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
#[allow(dead_code)]
mod recorder;
mod wav;

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    if let Err(error) = gui::run() {
        gui::show_error(None, &format!("无法启动录音程序：{error:#}"));
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(not(windows))]
    {
        eprintln!("本程序仅支持 Windows");
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(windows)]
    std::process::ExitCode::SUCCESS
}
