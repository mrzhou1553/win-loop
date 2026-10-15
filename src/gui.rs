use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_LIMITTEXT, EM_SETSEL};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::{applications, recorder};

const TARGET: usize = 101;
const REFRESH: usize = 102;
const DIRECTORY: usize = 103;
const BROWSE: usize = 104;
const NAME: usize = 105;
const START: usize = 106;
const STOP: usize = 107;
const STATUS: usize = 108;
const PREVIEW: usize = 109;
const ELAPSED: usize = 110;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

struct Recording {
    running: Arc<AtomicBool>,
    worker: JoinHandle<Result<()>>,
    started: Receiver<Instant>,
    since: Option<Instant>,
    path: PathBuf,
}

struct App {
    window: HWND,
    font: HFONT,
    processes: Vec<applications::AudioApplication>,
    recording: Option<Recording>,
    closing: bool,
}

// 每个录音/枚举线程自行初始化并释放 COM，接口不会跨线程传递。
fn with_mta<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    wasapi::initialize_mta()
        .ok()
        .context("无法初始化音频线程")?;
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            wasapi::deinitialize();
        }
    }
    let _apartment = Apartment;
    operation()
}

pub fn run() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let result = run_window();
    unsafe { CoUninitialize() };
    result
}

fn run_window() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let dpi = GetDpiForSystem() as i32;
        let scale = |value: i32| value * dpi / 96;
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("WinLoopRecorder"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            // MAKEINTRESOURCEW(1)：低 16 位表示资源 ID，并非字符串地址。
            hIcon: LoadIconW(Some(instance), PCWSTR(std::ptr::without_provenance(1)))?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut _),
            ..Default::default()
        };
        ensure!(RegisterClassW(&class) != 0, "无法注册录音窗口");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT {
            right: scale(740),
            bottom: scale(596),
            ..Default::default()
        };
        AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE::default())?;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let window = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.lpszClassName,
            w!("win-loop · 音频录制"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )?;
        // 显示前在所在屏幕的工作区内居中，避开任务栏。
        let mut monitor_info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(
            MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
            &mut monitor_info,
        )
        .ok()
        .context("无法获取显示器工作区")?;
        let work = monitor_info.rcWork;
        SetWindowPos(
            window,
            None,
            work.left + (work.right - work.left - width).max(0) / 2,
            work.top + (work.bottom - work.top - height).max(0) / 2,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .context("无法设置窗口位置")?;
        let font = CreateFontW(
            -scale(16),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Microsoft YaHei UI"),
        );
        let mut app = App {
            window,
            font,
            processes: Vec::new(),
            recording: None,
            closing: false,
        };
        app.create_controls(instance, dpi)?;
        app.refresh()?;
        app.update_preview();
        ensure!(
            SetTimer(Some(window), 1, 100, None) != 0,
            "无法启动界面计时器"
        );
        APP.with(|state| *state.borrow_mut() = Some(app));
        let _ = ShowWindow(window, SW_SHOW);
        let _ = SetFocus(Some(GetDlgItem(Some(window), TARGET as i32)?));
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            ensure!(result != -1, "读取窗口消息失败");
            if result == 0 {
                break;
            }
            if !IsDialogMessageW(window, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        APP.with(|state| state.borrow_mut().take());
    }
    Ok(())
}

impl App {
    fn create_controls(&self, instance: HINSTANCE, dpi: i32) -> Result<()> {
        let directory = std::env::current_exe()?
            .parent()
            .context("无法确定程序目录")?
            .join("recordings")
            .display()
            .to_string();
        let controls = [
            (0, "STATIC", "音频录制", 0, 24, 18, 690, 28),
            (
                0,
                "STATIC",
                "选择声音来源，开始录制；停止后自动完成并保存 WAV 文件。",
                0,
                24,
                52,
                690,
                24,
            ),
            (
                0,
                "STATIC",
                "录制前请取消静音，并将播放软件自身音量、Windows 音量混合器中\r\n该软件的音量及 Windows 系统总音量均设置为大于 0。",
                0,
                24,
                84,
                692,
                48,
            ),
            (0, "STATIC", "录制来源", 0, 24, 92, 180, 24),
            (
                TARGET,
                "COMBOBOX",
                "",
                CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0,
                24,
                120,
                550,
                260,
            ),
            (
                REFRESH,
                "BUTTON",
                "刷新进程",
                WS_TABSTOP.0,
                590,
                118,
                126,
                32,
            ),
            (
                0,
                "STATIC",
                "进程列表来自音频会话；未显示时请先让应用播放声音，再刷新。",
                0,
                24,
                162,
                692,
                24,
            ),
            (0, "STATIC", "保存目录", 0, 24, 196, 180, 24),
            (
                DIRECTORY,
                "EDIT",
                &directory,
                ES_AUTOHSCROLL as u32 | WS_BORDER.0 | WS_TABSTOP.0,
                24,
                224,
                550,
                30,
            ),
            (
                BROWSE,
                "BUTTON",
                "选择目录…",
                WS_TABSTOP.0,
                590,
                222,
                126,
                32,
            ),
            (0, "STATIC", "文件名（留空自动命名）", 0, 24, 270, 420, 24),
            (
                NAME,
                "EDIT",
                "",
                ES_AUTOHSCROLL as u32 | WS_BORDER.0 | WS_TABSTOP.0,
                24,
                298,
                550,
                30,
            ),
            (0, "STATIC", ".wav", 0, 590, 301, 126, 24),
            (
                0,
                "STATIC",
                "保存格式：WAV（未压缩音频）。后缀自动添加，无需输入。",
                0,
                24,
                340,
                692,
                24,
            ),
            (PREVIEW, "STATIC", "", 0, 24, 370, 692, 46),
            (
                START,
                "BUTTON",
                "开始录制",
                BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0,
                24,
                428,
                160,
                40,
            ),
            (STOP, "BUTTON", "停止录制", WS_TABSTOP.0, 200, 428, 160, 40),
            (
                ELAPSED,
                "STATIC",
                "录制时长  00:00:00",
                0,
                480,
                438,
                236,
                26,
            ),
            (STATUS, "STATIC", "就绪", 0, 24, 486, 692, 44),
        ];
        for (id, class, text, style, x, y, width, height) in controls {
            // 为顶部常驻的音量提示留出空间。
            let y = if y >= 92 { y + 56 } else { y };
            let class = wide(class);
            let text = wide(text);
            unsafe {
                let control = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    PCWSTR(class.as_ptr()),
                    PCWSTR(text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                    x * dpi / 96,
                    y * dpi / 96,
                    width * dpi / 96,
                    height * dpi / 96,
                    Some(self.window),
                    Some(HMENU(id as *mut _)),
                    Some(instance),
                    None,
                )?;
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(self.font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
        self.enable(STOP, false);
        self.send(NAME, EM_LIMITTEXT, 251, 0);
        Ok(())
    }

    fn control(&self, id: usize) -> HWND {
        unsafe { GetDlgItem(Some(self.window), id as i32).unwrap_or_default() }
    }

    fn send(&self, id: usize, message: u32, wparam: usize, lparam: isize) -> LRESULT {
        unsafe {
            SendMessageW(
                self.control(id),
                message,
                Some(WPARAM(wparam)),
                Some(LPARAM(lparam)),
            )
        }
    }

    fn text(&self, id: usize) -> String {
        unsafe {
            let control = self.control(id);
            let mut buffer = vec![0; GetWindowTextLengthW(control) as usize + 1];
            let count = GetWindowTextW(control, &mut buffer);
            String::from_utf16_lossy(&buffer[..count as usize])
        }
    }

    fn set_text(&self, id: usize, value: &str) {
        let value = wide(value);
        unsafe {
            let _ = SetWindowTextW(self.control(id), PCWSTR(value.as_ptr()));
        }
    }

    fn enable(&self, id: usize, enabled: bool) {
        unsafe {
            let _ = EnableWindow(self.control(id), enabled);
        }
    }

    fn refresh(&mut self) -> Result<()> {
        let previous = self.selected_pid();
        self.processes = std::thread::spawn(|| {
            with_mta(|| {
                applications::audio_applications().map(|items| items.into_values().collect())
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("刷新进程线程异常退出"))??;
        self.send(TARGET, CB_RESETCONTENT, 0, 0);
        let all = wide("全部声音（默认输出设备，含系统声音）");
        self.send(TARGET, CB_ADDSTRING, 0, all.as_ptr() as isize);
        let mut selected = 0;
        for (index, process) in self.processes.iter().enumerate() {
            let devices = process
                .devices
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            let state = if process.active { "活动" } else { "空闲" };
            let label = wide(&format!(
                "{} · PID {} · {state} · {devices}",
                process.name, process.pid
            ));
            self.send(TARGET, CB_ADDSTRING, 0, label.as_ptr() as isize);
            if previous == Some(process.pid) {
                selected = index + 1;
            }
        }
        self.send(TARGET, CB_SETCURSEL, selected, 0);
        self.send(TARGET, CB_SETDROPPEDWIDTH, 690, 0);
        self.set_text(
            STATUS,
            &format!("就绪 · 找到 {} 个有音频会话的进程", self.processes.len()),
        );
        Ok(())
    }

    fn selected_pid(&self) -> Option<u32> {
        let index = self.send(TARGET, CB_GETCURSEL, 0, 0).0;
        if index <= 0 {
            return None;
        }
        self.processes
            .get(index as usize - 1)
            .map(|process| process.pid)
    }

    fn update_preview(&self) {
        let name = self.text(NAME);
        let name = if name.is_empty() {
            "recording-<时间戳>"
        } else {
            &name
        };
        let path = PathBuf::from(self.text(DIRECTORY)).join(format!("{name}.wav"));
        self.set_text(PREVIEW, &format!("保存到：{}", path.display()));
    }

    fn choose_directory(&self) -> Result<()> {
        unsafe {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(
                dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR,
            )?;
            dialog.SetTitle(w!("选择 WAV 录音保存目录"))?;
            let current = wide(&self.text(DIRECTORY));
            if let Ok(folder) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(current.as_ptr()), None)
            {
                dialog.SetFolder(&folder)?;
            }
            if let Err(error) = dialog.Show(Some(self.window)) {
                if error.code() == windows::core::HRESULT::from_win32(1223) {
                    return Ok(());
                }
                return Err(error.into());
            }
            let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            // Shell 返回的字符串由 COM 分配，拷贝后必须释放。
            let directory = String::from_utf16_lossy(path.as_wide());
            CoTaskMemFree(Some(path.0.cast()));
            self.set_text(DIRECTORY, &directory);
        }
        self.update_preview();
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        if self.recording.is_some() {
            return Ok(());
        }
        let path = output_path(&self.text(DIRECTORY), &self.text(NAME))?;
        ensure!(
            !path.exists(),
            "文件已存在，请修改文件名；已有录音不会被覆盖"
        );
        let pid = self.selected_pid();
        let running = Arc::new(AtomicBool::new(true));
        let signal = Arc::clone(&running);
        let output = path.clone();
        let (sender, started) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("audio-recorder".into())
            .spawn(move || {
                with_mta(|| {
                    let on_started = || {
                        let _ = sender.send(Instant::now());
                    };
                    match pid {
                        Some(pid) => {
                            let target = applications::open_process(pid)?;
                            recorder::record_process_v2(
                                &output,
                                target.pid,
                                &target.name,
                                &signal,
                                &on_started,
                            )
                        }
                        None => recorder::record_v2(&output, &signal, &on_started),
                    }
                })
            })
            .context("无法启动录音线程")?;
        self.recording = Some(Recording {
            running,
            worker,
            started,
            since: None,
            path,
        });
        self.set_busy(true);
        self.set_text(STATUS, "正在初始化音频采集…");
        self.set_text(ELAPSED, "录制时长  00:00:00");
        self.set_text(
            PREVIEW,
            &format!(
                "保存到：{}",
                self.recording.as_ref().unwrap().path.display()
            ),
        );
        Ok(())
    }

    fn set_busy(&self, busy: bool) {
        for id in [TARGET, REFRESH, DIRECTORY, BROWSE, NAME, START] {
            self.enable(id, !busy);
        }
        self.enable(STOP, busy);
    }

    fn stop(&self) {
        if let Some(recording) = &self.recording {
            recording.running.store(false, Ordering::Relaxed);
            self.enable(STOP, false);
            self.set_text(STATUS, "正在停止录制并保存 WAV，请稍候…");
        }
    }

    fn tick(&mut self) {
        let Some(recording) = &mut self.recording else {
            return;
        };
        if let Ok(since) = recording.started.try_recv() {
            recording.since = Some(since);
        }
        let seconds = recording.since.map_or(0, |since| since.elapsed().as_secs());
        let active = recording.running.load(Ordering::Relaxed);
        let finished = recording.worker.is_finished();
        self.set_text(
            ELAPSED,
            &format!(
                "录制时长  {:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            ),
        );
        if !finished {
            if active && self.recording.as_ref().unwrap().since.is_some() {
                self.set_text(STATUS, "正在录制 · 点击“停止录制”完成并保存文件");
            }
            return;
        }
        let recording = self.recording.take().unwrap();
        let result = recording
            .worker
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("录音线程异常退出")));
        self.set_busy(false);
        match result {
            Ok(()) => self.set_text(STATUS, "已保存 WAV 文件 · 可以开始下一次录制"),
            Err(error) => {
                self.set_text(STATUS, "录制失败，请查看错误信息并重试。");
                show_error(
                    Some(self.window),
                    &format!(
                        "录制失败：{error:#}\n保存路径：{}",
                        recording.path.display()
                    ),
                );
            }
        }
        if self.closing {
            unsafe {
                let _ = DestroyWindow(self.window);
            }
        }
    }

    fn command(&mut self, id: usize, notification: u16) -> Result<()> {
        if notification == EN_CHANGE as u16 && [NAME, DIRECTORY].contains(&id) {
            if id == NAME {
                let name = self.text(NAME);
                let stem = without_wav_suffix(&name);
                if stem != name {
                    self.set_text(NAME, stem);
                    self.send(
                        NAME,
                        EM_SETSEL,
                        stem.encode_utf16().count(),
                        stem.encode_utf16().count() as isize,
                    );
                }
            }
            if self.recording.is_none() {
                self.update_preview();
            }
        } else if notification == BN_CLICKED as u16 {
            match id {
                REFRESH if self.recording.is_none() => self.refresh()?,
                BROWSE if self.recording.is_none() => self.choose_directory()?,
                START if !self.closing => self.start()?,
                STOP => self.stop(),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(recording) = self.recording.take() {
            recording.running.store(false, Ordering::Relaxed);
            let _ = recording.worker.join();
        }
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.font.0));
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    let handled = APP.with(|state| {
        // 控件修改文本、弹出目录对话框时可能重入消息循环。
        let Ok(mut state) = state.try_borrow_mut() else {
            return false;
        };
        let Some(app) = state.as_mut() else {
            return false;
        };
        match message {
            WM_COMMAND => {
                let result = app.command(wparam.0 & 0xffff, ((wparam.0 >> 16) & 0xffff) as u16);
                if let Err(error) = result {
                    show_error(Some(window), &format!("{error:#}"));
                }
            }
            WM_TIMER => app.tick(),
            WM_CLOSE => {
                if app.recording.is_some() {
                    app.closing = true;
                    app.stop();
                } else {
                    unsafe {
                        let _ = DestroyWindow(window);
                    }
                }
            }
            _ => return false,
        }
        true
    });
    if handled {
        LRESULT(0)
    } else {
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }
}

pub fn show_error(window: Option<HWND>, text: &str) {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            window,
            PCWSTR(text.as_ptr()),
            w!("win-loop · 提示"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn without_wav_suffix(mut name: &str) -> &str {
    while name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".wav"))
    {
        name = &name[..name.len() - 4];
    }
    name
}

fn output_path(directory: &str, name: &str) -> Result<PathBuf> {
    ensure!(!directory.trim().is_empty(), "请选择或输入保存目录");
    let name = without_wav_suffix(name);
    let name = if name.is_empty() {
        format!(
            "recording-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
        )
    } else {
        ensure!(!name.ends_with([' ', '.']), "文件名不能以空格或句点结尾");
        ensure!(
            !name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
            "文件名不能包含路径或 <> : \" / \\ | ? * 等字符"
        );
        ensure!(name.encode_utf16().count() <= 251, "文件名过长");
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
        let numbered = ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        ensure!(
            !reserved.contains(&stem.as_str()) && !numbered,
            "文件名是 Windows 保留名称，请更换"
        );
        name.to_owned()
    };
    std::path::absolute(Path::new(directory.trim()).join(format!("{name}.wav")))
        .context("保存目录无效")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_name_has_one_fixed_wav_extension() -> Result<()> {
        let path = output_path("recordings", "中文会议.v1.WAV.wav")?;
        assert_eq!(path.file_name().unwrap(), "中文会议.v1.wav");
        assert!(path.is_absolute());
        Ok(())
    }

    #[test]
    fn unsafe_windows_file_names_are_rejected() {
        for name in [
            "../escape",
            "a\\b",
            "bad:name",
            "NUL",
            "CON.txt",
            "COM1",
            "LPT²",
            "end.",
            "end ",
            "bad\nname",
        ] {
            assert!(output_path("recordings", name).is_err(), "{name}");
        }
        assert!(output_path("", "name").is_err());
    }

    #[test]
    fn blank_name_is_generated_automatically() -> Result<()> {
        let path = output_path("recordings", "")?;
        assert!(
            path.file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("recording-")
        );
        assert_eq!(path.extension().unwrap(), "wav");
        Ok(())
    }
}
