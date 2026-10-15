use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use wasapi::{DeviceEnumerator, Direction, SessionState};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

pub struct TargetProcess {
    pub pid: u32,
    pub name: String,
    // 选定后一直持有进程句柄，直到录音结束；不关闭或修改目标进程。
    _handle: OwnedHandle,
}

pub enum RecordingTarget {
    All,
    Process(TargetProcess),
}

pub struct AudioApplication {
    pub pid: u32,
    pub name: String,
    pub active: bool,
    pub devices: BTreeSet<String>,
}

pub fn select_target(running: &AtomicBool) -> Result<Option<RecordingTarget>> {
    print_applications()?;
    println!("请输入要录制的 PID，或 ALL 录制默认输出设备的全部声音；r 刷新列表，q 退出。");
    println!("也可输入应用主进程的 PID，以包含它的音频子进程。");
    let input = stdin_lines();
    while running.load(Ordering::Relaxed) {
        print!("PID > ");
        io::stdout().flush()?;
        let Some(line) = wait_for_line(&input, running)? else {
            println!("\n已取消，未开始录音。");
            return Ok(None);
        };
        match line.trim() {
            value if value.eq_ignore_ascii_case("all") => return Ok(Some(RecordingTarget::All)),
            "q" | "Q" => return Ok(None),
            "r" | "R" => print_applications()?,
            value => match value.parse::<u32>() {
                Ok(pid) if pid != 0 && pid != std::process::id() => match open_process(pid) {
                    Ok(target) => return Ok(Some(RecordingTarget::Process(target))),
                    Err(error) => {
                        eprintln!("无法选择 PID {pid}：{error:#}。请重新输入或用 r 刷新。")
                    }
                },
                _ => eprintln!("请输入有效的正整数 PID（不能选择本程序），或输入 ALL / r / q。"),
            },
        }
    }
    println!("\n已取消，未开始录音。");
    Ok(None)
}

fn print_applications() -> Result<()> {
    let applications = audio_applications()?;
    println!("\nALL：录制默认输出设备上的全部声音，包括应用混音和系统提示音。");
    println!("\n当前有音频会话的应用：");
    println!("{:<10} {:<28} {:<12} 输出设备", "PID", "进程名", "会话状态");
    for app in applications.values() {
        let state = if app.active { "活动" } else { "空闲" };
        let devices = app.devices.iter().cloned().collect::<Vec<_>>().join("、");
        println!("{:<10} {:<28} {:<12} {}", app.pid, app.name, state, devices);
    }
    if applications.is_empty() {
        println!("未发现应用音频会话。请先让目标应用播放声音，再输入 r 刷新。");
    } else {
        println!("活动表示音频流已启动，空闲表示会话存在但未播放；列表不包含系统声音会话。");
    }
    Ok(())
}

pub fn audio_applications() -> Result<BTreeMap<u32, AudioApplication>> {
    let enumerator = DeviceEnumerator::new()?;
    let devices = enumerator.get_device_collection(&Direction::Render)?;
    let mut applications = BTreeMap::new();
    for device in &devices {
        let result = (|| -> Result<()> {
            let device = device?;
            let name = device.get_friendlyname()?;
            let manager = device.get_iaudiosessionmanager()?;
            let sessions = manager.get_audiosessionenumerator()?;
            for index in 0..sessions.get_count()? {
                // 枚举期间会话可能退出，忽略该会话后继续读取其他应用。
                let Ok(session) = sessions.get_session(index) else {
                    continue;
                };
                let Ok(state) = session.get_state() else {
                    continue;
                };
                if state == SessionState::Expired {
                    continue;
                }
                let Ok(pid) = session.get_process_id() else {
                    continue;
                };
                if pid == 0 || pid == std::process::id() {
                    continue;
                }
                let Ok(target) = open_process(pid) else {
                    continue;
                };
                let app = applications.entry(pid).or_insert_with(|| AudioApplication {
                    pid,
                    name: target.name,
                    active: false,
                    devices: BTreeSet::new(),
                });
                app.active |= state == SessionState::Active;
                app.devices.insert(name.clone());
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("读取某个输出设备的音频会话失败，继续枚举：{error:#}");
        }
    }
    Ok(applications)
}

pub fn open_process(pid: u32) -> Result<TargetProcess> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .context("进程已退出、不存在或无权读取")?;
    // OpenProcess 返回的句柄由 OwnedHandle 独占，在所有返回路径自动关闭。
    let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut exit_code = 0;
    unsafe {
        GetExitCodeProcess(handle, &mut exit_code)?;
    }
    ensure!(exit_code == 259, "进程已退出"); // STILL_ACTIVE
    let mut buffer = vec![0_u16; 32768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?;
    }
    let image = String::from_utf16_lossy(&buffer[..length as usize]);
    let name = Path::new(&image)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok(TargetProcess {
        pid,
        name,
        _handle: owned,
    })
}

fn stdin_lines() -> Receiver<io::Result<String>> {
    let (sender, receiver) = mpsc::channel();
    // 把阻塞的终端读取留在独立线程，主线程在输入 PID 时也能响应 Ctrl+C。
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn wait_for_line(
    input: &Receiver<io::Result<String>>,
    running: &AtomicBool,
) -> Result<Option<String>> {
    while running.load(Ordering::Relaxed) {
        match input.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => return Ok(Some(line.context("读取 PID 失败")?)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(None)
}
