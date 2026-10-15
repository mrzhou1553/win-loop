# win-loop

[简体中文](README.md) | **English**

A Windows audio recorder written in Rust. Capture audio from one application and its child processes, or record the mix from the default output device, directly to a WAV file using WASAPI and hound.

The default executable opens a native Windows GUI without a console window. A separate command-line executable is also available. The GUI and runtime messages currently use Chinese; this document explains their operation in English.

## Features

- Record a selected process and its descendants while excluding other applications.
- Record all playback audio routed to the default Windows output device.
- Refresh the application list, choose an output folder, and use custom filenames, including Chinese characters.
- Use the CLI to enter a PID manually and stop recording with Ctrl+C.
- Stream uncompressed 32-bit floating-point audio to WAV with periodic header updates.
- Preserve the recording timeline with silence when audio packets are unavailable.
- Finalize recordings on stop or GUI window close, and never overwrite existing files.

No virtual audio driver or FFmpeg is required. Microphone input is not recorded.

## Requirements

- Windows build 20348 or later for process loopback capture, such as Windows 11.
- Rust 1.88 or later to build the project.
- For Windows MSVC builds: the MSVC C++ build tools and the Windows SDK resource compiler, which embeds the application icon.

Python is only needed when regenerating the icon assets, not for normal Rust builds.

## Build and run

From the project directory, run the GUI during development:

```powershell
cargo run
```

Build optimized executables:

```powershell
cargo build --release
```

Then double-click `target/release/win-loop.exe`. The CLI executable is `target/release/win-loop-cli.exe`.

### Using the GUI

1. In **录制来源** (recording source), select **全部声音** (all audio) or the application to record. Process recording includes child processes.
2. Use **刷新进程** (refresh processes) to update the audio-session list. If an application is missing, let it play some audio and refresh again.
3. Click **选择目录…** (choose folder), or type an output directory. By default, recordings go into a `recordings` folder beside the executable. Missing folders are created automatically.
4. Enter a filename, or leave it blank for `recording-<Unix timestamp in milliseconds>`. The GUI adds `.wav` automatically and removes an already-entered WAV suffix. Invalid Windows filenames are rejected.
5. Click **开始录制** (start recording). The GUI shows initialization status, elapsed time, and the actual output path. Source, folder, and filename controls are locked during recording.
6. Click **停止录制** (stop recording), and wait for **已保存 WAV 文件** (WAV file saved). Closing the window during recording also stops capture, drains the remaining buffer, and finalizes the WAV header before exiting.

Existing files are never overwritten. Choose a new filename before recording again to the same directory. Initialization and capture errors are reported in the GUI.

### Using the CLI

```powershell
cargo run --bin win-loop-cli
cargo run --bin win-loop-cli -- "recordings/my-recording.wav"
cargo run --bin win-loop-cli -- --help
```

After startup, enter a process ID to record that process and its descendants, `ALL` to capture the default output device, `r` to refresh the list, or `q` to quit. Press Ctrl+C during recording to stop and save.

You can enter an application's main-process PID manually, even when it does not appear in the audio-session list. With no output argument, the CLI saves into `recordings` under the current working directory. Output paths must have a `.wav` extension.

## Recording behavior and limitations

| Mode | Source | WAV format |
| --- | --- | --- |
| Selected process | The selected PID and its child processes; not tied to a specific output device | 48 kHz, stereo, 32-bit float |
| All audio / `ALL` | The default output device's playback mix | Device sample rate and channel count, 32-bit float |

- In all-audio mode, audio sent to other output devices is not captured. A recording does not automatically follow changes to the default device.
- Browsers and similar applications may play audio through child processes. Multiple tabs in the same process tree may be recorded together; this is not a per-tab recorder.
- If the target is silent or exits, recording continues with silence until you stop it. A new PID after an application restart is not tracked automatically; select the application again.
- The recorder writes a readable empty WAV first, then streams samples through a bounded write buffer. It updates the header and flushes every five seconds, and explicitly finalizes the file on stop.
- Silence preserves elapsed time when WASAPI supplies no packets. Lost audio cannot be recovered. The CLI reports detected discontinuities.
- Capture errors trigger an attempt to finalize the existing file. Abrupt termination may leave a file readable up to its last successful flush; power-loss recovery is not guaranteed.
- Standard RIFF WAV is limited to roughly 4 GiB of audio data. The recorder stops and finalizes the file when it reaches that limit; it does not split recordings automatically. At 48 kHz stereo float, audio uses about 1.38 GB per hour, allowing roughly 3.1 hours per file.

## Project layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | Default GUI entry point and Windows GUI subsystem configuration |
| `src/gui.rs` | Native controls, folder selection, filenames, recording thread, and shutdown handling |
| `src/cli.rs` | CLI arguments, Ctrl+C handling, and COM lifecycle |
| `src/applications.rs` | Audio-session enumeration, process names, and interactive source selection |
| `src/recorder.rs` | Process and device loopback capture, packet handling, silence, and stop logic |
| `src/wav.rs` | Buffered WAV writing, periodic flush, finalization, and size checks |
| `assets/` | Icon source, generated icon files, and the resource definition |
| `build.rs` | Embeds the icon into the GUI executable |

The `wasapi 0.26` high-level capture helper does not handle a potentially null data pointer for silent packets. This project obtains the raw capture interface through the public `as_iaudioclient()` API and uses the `windows` bindings for packet reads, handling silence before copying and releasing device buffers before writing to disk.

### Application icon

The GUI executable embeds its file, window, and taskbar icon. No separate icon file is needed when distributing the executable.

The source is `assets/win-loop.svg`; the multi-size icon is `assets/win-loop.ico`. To replace the icon, update the ICO and rebuild:

```powershell
cargo build --release --bin win-loop
```

After editing the existing SVG, you can regenerate the PNG preview and ICO using Python with Pillow installed:

```powershell
python assets/generate_icon.py
```

## Development checks

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Existing unit tests cover WAV samples and silence, continued writes after flushing, finalization, protection against overwriting files, invalid sample frames, oversized recordings, Chinese filenames, automatic naming, WAV suffix handling, and invalid Windows filenames.

The original Chinese README also records previous manual audio-isolation and GUI checks. Hardware capture and GUI behavior still need manual verification on the target Windows environment; the unit tests do not exercise live audio capture.

## Contributing

Bug reports and improvements are welcome through GitHub issues and pull requests. For recording problems, include your Windows build, recording mode, output device, and error message so the issue can be reproduced.
