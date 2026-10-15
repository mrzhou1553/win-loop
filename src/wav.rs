use std::fs::{self, File, OpenOptions};
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use hound::{SampleFormat, WavSpec, WavWriter};

// 留出文件头空间，避免普通 RIFF WAV 的 32 位长度溢出。
const MAX_DATA_BYTES: u64 = u32::MAX as u64 - 4096;

pub struct WavOutput {
    writer: WavWriter<BufWriter<File>>,
    channels: u16,
    frames: u64,
}

impl WavOutput {
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> Result<Self> {
        ensure!(sample_rate > 0 && channels > 0, "无效的音频格式");
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).context("无法创建输出目录")?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("无法创建 {}（已有文件不会被覆盖）", path.display()))?;
        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::new(BufWriter::with_capacity(256 * 1024, file), spec)?;
        // 即使当前还没有声音，也先创建一个可读取的空 WAV。
        writer.flush()?;
        Ok(Self {
            writer,
            channels,
            frames: 0,
        })
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    fn check_size(&self, extra_frames: u64) -> Result<()> {
        ensure!(
            self.frames.saturating_add(extra_frames)
                <= MAX_DATA_BYTES / (u64::from(self.channels) * 4),
            "录音已达到普通 WAV 的约 4 GiB 上限，停止并保存；请开始新的录音"
        );
        Ok(())
    }

    pub fn pad_to(&mut self, target_frame: u64) -> Result<()> {
        let missing = target_frame.saturating_sub(self.frames);
        self.check_size(missing)?;
        for _ in 0..missing {
            for _ in 0..self.channels {
                self.writer.write_sample(0.0_f32)?;
            }
            self.frames += 1;
        }
        Ok(())
    }

    pub fn write_packet(&mut self, bytes: &[u8]) -> Result<()> {
        let frame_size = usize::from(self.channels) * 4;
        ensure!(
            bytes.len().is_multiple_of(frame_size),
            "音频数据不是完整的采样帧"
        );
        let frame_count = (bytes.len() / frame_size) as u64;
        self.check_size(frame_count)?;
        for sample in bytes.chunks_exact(4) {
            self.writer
                .write_sample(f32::from_le_bytes(sample.try_into()?))?;
        }
        self.frames += frame_count;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("刷新 WAV 文件失败")
    }

    pub fn finalize(self) -> Result<()> {
        self.writer.finalize().context("WAV 文件收尾失败")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(label: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "win-loop-{}-{timestamp}-{label}.wav",
            std::process::id()
        ))
    }

    #[test]
    fn checkpoint_and_finalize_preserve_samples_and_silence() -> Result<()> {
        let path = test_path("samples");
        let mut output = WavOutput::create(&path, 48_000, 2)?;
        let bytes: Vec<u8> = [0.25_f32, -0.5, 0.75, -1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        output.write_packet(&bytes)?;
        output.pad_to(4)?;
        output.flush()?;
        let checkpoint = hound::WavReader::open(&path)?;
        assert_eq!(checkpoint.duration(), 4);
        drop(checkpoint);
        output.write_packet(&bytes)?;
        output.finalize()?;
        let mut reader = hound::WavReader::open(&path)?;
        assert_eq!(reader.spec().sample_format, SampleFormat::Float);
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), 6);
        let samples = reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(
            samples,
            [
                0.25, -0.5, 0.75, -1.0, 0.0, 0.0, 0.0, 0.0, 0.25, -0.5, 0.75, -1.0
            ]
        );
        drop(reader);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn existing_file_is_not_overwritten() -> Result<()> {
        let path = test_path("existing");
        fs::write(&path, b"keep this")?;
        assert!(WavOutput::create(&path, 48_000, 2).is_err());
        assert_eq!(fs::read(&path)?, b"keep this");
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn incomplete_frame_and_oversized_recording_are_rejected() -> Result<()> {
        let path = test_path("limits");
        let mut output = WavOutput::create(&path, 48_000, 2)?;
        assert!(output.write_packet(&[0; 4]).is_err());
        assert!(output.pad_to(u64::MAX).is_err());
        assert_eq!(output.frames(), 0);
        output.finalize()?;
        assert_eq!(hound::WavReader::open(&path)?.duration(), 0);
        fs::remove_file(path)?;
        Ok(())
    }
}
