use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use wasapi::{
    AudioClient, BufferInfo, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};
use windows::Win32::Media::Audio::IAudioCaptureClient;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::wav::WavOutput;

// ALL 模式：录制默认输出设备上的全部声音。
pub fn record(path: &Path, running: &AtomicBool) -> Result<()> {
    record_v2(path, running, &|| {})
}

pub fn record_v2(path: &Path, running: &AtomicBool, on_started: &dyn Fn()) -> Result<()> {
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .context("找不到 Windows 默认输出设备，请检查扬声器或耳机")?;
    let mut client = device.get_iaudioclient()?;
    let mix = client.get_mixformat()?;
    let rate = mix.get_samplespersec();
    let channels = mix.get_nchannels();
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        rate as usize,
        channels as usize,
        Some(mix.get_dwchannelmask()),
    );
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 1_000_000, // 100 ms，给缓冲写入留出空间。
            },
        )
        .context("无法初始化 WASAPI 回环采集")?;
    let event = client.set_get_eventhandle()?;
    // wasapi 0.26 的读取助手未处理 SILENT 时可能为空的指针。
    // 通过它公开的原始接口读取，并在 read_packet 中安全处理静音。
    let capture: IAudioCaptureClient = unsafe { client.as_iaudioclient().GetService()? };
    let clock = Clock::new()?;
    let mut output = WavOutput::create(path, rate, channels)?;
    println!("录制设备：{}", device.get_friendlyname()?);
    println!("WAV 格式：{rate} Hz，{channels} 声道，32 位浮点");
    println!("保存路径：{}", std::path::absolute(path)?.display());
    println!("正在录音，按 Ctrl+C 停止并保存。");

    let capture_result = capture_loop(
        &mut client,
        &capture,
        &event,
        &clock,
        &format,
        running,
        &mut output,
        on_started,
    );
    let frames = output.frames();
    // 采集或设备出现错误也尽力完成 WAV 文件头。
    let finalize_result = output.finalize();
    finalize_result?;
    println!(
        "已保存：{}（{:.2} 秒）",
        path.display(),
        frames as f64 / f64::from(rate)
    );
    capture_result
}

pub fn record_process(path: &Path, pid: u32, name: &str, running: &AtomicBool) -> Result<()> {
    record_process_v2(path, pid, name, running, &|| {})
}

pub fn record_process_v2(
    path: &Path,
    pid: u32,
    name: &str,
    running: &AtomicBool,
    on_started: &dyn Fn(),
) -> Result<()> {
    // 进程回环不绑定声卡，GetMixFormat 不可用，显式选择采集格式。
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(pid, true)
        .context("无法启用进程音频采集（需要 Windows Build 20348 或更新版本）")?;
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )
        .context("无法初始化进程音频采集流")?;
    let event = client.set_get_eventhandle()?;
    let capture: IAudioCaptureClient = unsafe { client.as_iaudioclient().GetService()? };
    let clock = Clock::new()?;
    let mut output = WavOutput::create(path, 48_000, 2)?;
    println!("录制应用：{name}（PID {pid}，包含子进程）");
    println!("WAV 格式：48000 Hz，2 声道，32 位浮点");
    println!("保存路径：{}", std::path::absolute(path)?.display());
    println!("正在录音，按 Ctrl+C 停止并保存。");

    let capture_result = capture_loop(
        &mut client,
        &capture,
        &event,
        &clock,
        &format,
        running,
        &mut output,
        on_started,
    );
    let frames = output.frames();
    output.finalize()?;
    println!(
        "已保存：{}（{:.2} 秒）",
        path.display(),
        frames as f64 / 48_000.0
    );
    capture_result
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    client: &mut AudioClient,
    capture: &IAudioCaptureClient,
    event: &wasapi::Handle,
    clock: &Clock,
    format: &WaveFormat,
    running: &AtomicBool,
    output: &mut WavOutput,
    on_started: &dyn Fn(),
) -> Result<()> {
    let rate = format.get_samplespersec();
    let channels = format.get_nchannels();
    let start = clock.now()?;
    client.start_stream().context("无法启动音频采集")?;
    on_started();
    let mut bytes = Vec::new();
    let mut checkpoint = Instant::now();
    let result = (|| -> Result<()> {
        while running.load(Ordering::Relaxed) {
            drain_packets(capture, &mut bytes, channels, start, rate, output)?;
            // 没有应用播放声音时，WASAPI 可能没有数据包；保留时间轴上的静音。
            // 留出 200 ms 余量，避免在尚未到达的数据包之前写入静音。
            let idle_until = clock.now()?.saturating_sub(2_000_000);
            output.pad_to(frame_at(idle_until, start, rate))?;
            if checkpoint.elapsed() >= Duration::from_secs(5) {
                output.flush()?;
                checkpoint = Instant::now();
            }
            match event.wait_for_event(50) {
                Ok(()) | Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    let end = clock.now();
    let stop_result = client.stop_stream().context("停止音频采集失败");
    result?;
    stop_result?;
    // Stop 不会清空缓冲，读完最后的数据包后再补齐尾部静音。
    drain_packets(capture, &mut bytes, channels, start, rate, output)?;
    output.pad_to(frame_at(end?, start, rate))?;
    Ok(())
}

fn drain_packets(
    capture: &IAudioCaptureClient,
    bytes: &mut Vec<u8>,
    channels: u16,
    start: u64,
    rate: u32,
    output: &mut WavOutput,
) -> Result<()> {
    while unsafe { capture.GetNextPacketSize()? } > 0 {
        let (frames, info) = read_packet(capture, bytes, usize::from(channels) * 4)?;
        if frames == 0 {
            break;
        }
        if info.flags.data_discontinuity && output.frames() > 0 {
            eprintln!("提示：检测到音频数据不连续，缺失位置尽可能补为静音。");
        }
        if !info.flags.timestamp_error {
            let packet_start = frame_at(info.timestamp, start, rate);
            output.pad_to(packet_start)?;
            // 已经补为静音的时间段或开始前的采样，不重复写入。
            let skip = output
                .frames()
                .saturating_sub(packet_start)
                .min(u64::from(frames));
            let skip_bytes = skip as usize * usize::from(channels) * 4;
            output.write_packet(&bytes[skip_bytes..])?;
        } else {
            output.write_packet(bytes)?;
        }
    }
    Ok(())
}

fn read_packet(
    capture: &IAudioCaptureClient,
    bytes: &mut Vec<u8>,
    frame_size: usize,
) -> Result<(u32, BufferInfo)> {
    let mut data = std::ptr::null_mut();
    let (mut frames, mut flags, mut index, mut timestamp) = (0, 0, 0, 0);
    unsafe {
        capture.GetBuffer(
            &mut data,
            &mut frames,
            &mut flags,
            Some(&mut index),
            Some(&mut timestamp),
        )?;
    }
    let info = BufferInfo::new(flags, index, timestamp);
    if frames == 0 {
        return Ok((0, info));
    }
    let result = (|| -> Result<()> {
        bytes.resize(frames as usize * frame_size, 0);
        if info.flags.silent {
            bytes.fill(0);
        } else {
            ensure!(!data.is_null(), "WASAPI 返回了无效的音频指针");
            // 缓冲仅在 GetBuffer / ReleaseBuffer 之间有效，先复制到自己的内存。
            let length = bytes.len();
            bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(data, length) });
        }
        Ok(())
    })();
    let release = unsafe { capture.ReleaseBuffer(frames) };
    result?;
    release?;
    Ok((frames, info))
}

fn frame_at(timestamp: u64, start: u64, rate: u32) -> u64 {
    (u128::from(timestamp.saturating_sub(start)) * u128::from(rate) / 10_000_000) as u64
}

struct Clock {
    frequency: i64,
}

impl Clock {
    fn new() -> Result<Self> {
        let mut frequency = 0;
        unsafe {
            QueryPerformanceFrequency(&mut frequency)?;
        }
        if frequency <= 0 {
            bail!("无法读取 Windows 高精度时钟");
        }
        Ok(Self { frequency })
    }

    fn now(&self) -> Result<u64> {
        let mut ticks = 0;
        unsafe {
            QueryPerformanceCounter(&mut ticks)?;
        }
        Ok((ticks as u128 * 10_000_000 / self.frequency as u128) as u64)
    }
}
