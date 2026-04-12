use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStderr, ChildStdin, Command, Stdio},
    sync::OnceLock,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result};
use qemu_display::{Audio, AudioOutHandler, Display, PCMInfo, Volume};
use zbus::Connection;

pub(super) async fn register_audio_output(
    connection: &Connection,
    owner: &str,
    vm_name: &str,
) -> Result<Option<AudioSession>> {
    let display = Display::new(connection, Some(owner.to_owned()))
        .await
        .context("failed to open the QEMU display object for audio playback")?;
    let Some(mut audio) = display
        .audio()
        .await
        .context("failed to inspect the QEMU audio interface")?
    else {
        return Ok(None);
    };

    audio
        .register_out_listener(AudioPlaybackHandler::new(vm_name.to_owned()))
        .await
        .context("failed to register the audio playback listener with QEMU")?;

    Ok(Some(AudioSession { _audio: audio }))
}

pub(super) struct AudioSession {
    _audio: Audio,
}

struct AudioPlaybackHandler {
    vm_name: String,
    streams: HashMap<u64, AudioStreamHandle>,
}

impl AudioPlaybackHandler {
    fn new(vm_name: String) -> Self {
        Self {
            vm_name,
            streams: HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl AudioOutHandler for AudioPlaybackHandler {
    async fn init(&mut self, id: u64, info: PCMInfo) {
        self.streams
            .insert(id, AudioStreamHandle::spawn(self.vm_name.clone(), id, info));
    }

    async fn fini(&mut self, id: u64) {
        self.streams.remove(&id);
    }

    async fn set_enabled(&mut self, id: u64, enabled: bool) {
        if let Some(stream) = self.streams.get(&id) {
            stream.send(AudioCommand::SetEnabled(enabled));
        }
    }

    async fn set_volume(&mut self, id: u64, volume: Volume) {
        if let Some(stream) = self.streams.get(&id) {
            stream.send(AudioCommand::SetVolume(volume));
        }
    }

    async fn write(&mut self, id: u64, data: Vec<u8>) {
        if let Some(stream) = self.streams.get(&id) {
            stream.send(AudioCommand::Write(data));
        }
    }
}

struct AudioStreamHandle {
    tx: Sender<AudioCommand>,
}

impl AudioStreamHandle {
    fn spawn(vm_name: String, id: u64, info: PCMInfo) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread_name = format!("qd2-audio-{id}");
        let _ = thread::Builder::new().name(thread_name).spawn(move || {
            let mut worker = AudioStreamWorker::new(vm_name, id, info);
            worker.run(rx);
        });
        Self { tx }
    }

    fn send(&self, command: AudioCommand) {
        let _ = self.tx.send(command);
    }
}

enum AudioCommand {
    SetEnabled(bool),
    SetVolume(Volume),
    Write(Vec<u8>),
}

struct AudioStreamWorker {
    stream_label: String,
    info: PCMInfo,
    enabled: bool,
    volume: Volume,
    sink: Option<ProcessPlaybackSink>,
    candidates: Vec<AudioBackend>,
    next_candidate: usize,
    exhausted: bool,
}

impl AudioStreamWorker {
    fn new(vm_name: String, id: u64, info: PCMInfo) -> Self {
        let candidates = preferred_backends(&info);
        if candidates.is_empty() {
            eprintln!(
                "QD2 audio: {vm_name} stream {id} uses an unsupported PCM format ({})",
                describe_pcm_info(&info)
            );
        }

        Self {
            stream_label: format!("{vm_name} audio stream {id}"),
            info,
            enabled: true,
            volume: Volume {
                mute: false,
                volume: Vec::new(),
            },
            sink: None,
            candidates,
            next_candidate: 0,
            exhausted: false,
        }
    }

    fn run(&mut self, rx: Receiver<AudioCommand>) {
        while let Ok(command) = rx.recv() {
            match command {
                AudioCommand::SetEnabled(enabled) => self.enabled = enabled,
                AudioCommand::SetVolume(volume) => self.volume = volume,
                AudioCommand::Write(data) => self.write(data),
            }
        }
    }

    fn write(&mut self, data: Vec<u8>) {
        if !self.enabled || self.exhausted {
            return;
        }

        self.ensure_sink();
        let Some(sink) = self.sink.as_mut() else {
            return;
        };

        let data = apply_volume(&self.info, &self.volume, data);
        if let Err(error) = sink.write(&data) {
            eprintln!(
                "QD2 audio: {} backend `{}` failed: {error}",
                self.stream_label,
                sink.backend_name()
            );
            self.sink = None;
            self.next_candidate += 1;
            self.ensure_sink();
            if let Some(sink) = self.sink.as_mut() {
                if let Err(error) = sink.write(&data) {
                    eprintln!(
                        "QD2 audio: {} backend `{}` failed after fallback: {error}",
                        self.stream_label,
                        sink.backend_name()
                    );
                    self.sink = None;
                    self.next_candidate += 1;
                    self.exhausted = true;
                }
            }
        }
    }

    /// Open the first backend that accepts this PCM format so QEMU can stream
    /// audio without holding up the display listener path.
    fn ensure_sink(&mut self) {
        if self.sink.is_some() || self.exhausted {
            return;
        }

        while let Some(backend) = self.candidates.get(self.next_candidate).copied() {
            match ProcessPlaybackSink::spawn(backend, &self.info, &self.stream_label) {
                Ok(sink) => {
                    eprintln!(
                        "QD2 audio: {} using `{}` for {}",
                        self.stream_label,
                        backend.name(),
                        describe_pcm_info(&self.info)
                    );
                    self.sink = Some(sink);
                    return;
                }
                Err(error) => {
                    eprintln!(
                        "QD2 audio: could not start `{}` for {}: {error:#}",
                        backend.name(),
                        self.stream_label
                    );
                    self.next_candidate += 1;
                }
            }
        }

        self.exhausted = true;
    }
}

static AUDIO_ENVIRONMENT_HINT_EMITTED: OnceLock<()> = OnceLock::new();

struct ProcessPlaybackSink {
    backend: AudioBackend,
    child: Child,
    stdin: Option<ChildStdin>,
}

impl ProcessPlaybackSink {
    fn spawn(backend: AudioBackend, info: &PCMInfo, stream_label: &str) -> Result<Self> {
        let mut command = backend.command(info, stream_label)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", backend.name()))?;
        let stdin = child
            .stdin
            .take()
            .context("audio backend did not provide a stdin pipe")?;
        let stderr = child.stderr.take();

        if let Some(stderr) = stderr {
            spawn_stderr_monitor(backend, stream_label.to_owned(), stderr);
        } else {
            maybe_print_audio_environment_hint(backend, None);
        }

        Ok(Self {
            backend,
            child,
            stdin: Some(stdin),
        })
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.stdin
            .as_mut()
            .context("audio backend stdin is not available")?
            .write_all(data)
            .context("failed to write PCM data to the audio backend")
    }

    fn backend_name(&self) -> &'static str {
        self.backend.name()
    }
}

fn spawn_stderr_monitor(backend: AudioBackend, stream_label: String, stderr: ChildStderr) {
    let _ = thread::Builder::new()
        .name(format!("qd2-audio-stderr-{}", backend.name()))
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(std::result::Result::ok) {
                eprintln!(
                    "QD2 audio backend `{}` ({}): {}",
                    backend.name(),
                    stream_label,
                    line
                );
                maybe_print_audio_environment_hint(backend, Some(line.as_str()));
            }
        });
}

impl Drop for ProcessPlaybackSink {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

fn maybe_print_audio_environment_hint(backend: AudioBackend, stderr_line: Option<&str>) {
    if backend != AudioBackend::PwPlay {
        return;
    }

    let should_hint = match stderr_line {
        Some(line) => {
            line.contains("Failed to connect to PipeWire instance")
                || line.contains("Host is down")
                || line.contains("Operation not permitted")
        }
        None => true,
    };
    if !should_hint {
        return;
    }

    let Some(hint) = pipewire_sudo_environment_hint() else {
        return;
    };
    if AUDIO_ENVIRONMENT_HINT_EMITTED.set(()).is_ok() {
        eprintln!("{hint}");
    }
}

fn pipewire_sudo_environment_hint() -> Option<String> {
    pipewire_sudo_environment_hint_from(
        std::env::var("SUDO_USER").ok().as_deref(),
        std::env::var("SUDO_UID").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    )
}

fn pipewire_sudo_environment_hint_from(
    sudo_user: Option<&str>,
    sudo_uid: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    dbus_session_bus: Option<&str>,
) -> Option<String> {
    let sudo_user = sudo_user?;

    let runtime_matches_user_session = sudo_uid
        .zip(xdg_runtime_dir)
        .is_some_and(|(uid, runtime)| runtime == format!("/run/user/{uid}"));
    let dbus_matches_user_session = sudo_uid
        .zip(dbus_session_bus)
        .is_some_and(|(uid, address)| address.contains(&format!("/run/user/{uid}/bus")));

    if runtime_matches_user_session && dbus_matches_user_session {
        return None;
    }

    Some(format!(
        "QD2 audio hint: `pw-play` is running under sudo without the `{sudo_user}` desktop audio session environment. \
PipeWire playback usually needs the user session vars. Try rerunning with:\n  \
sudo --preserve-env=XDG_RUNTIME_DIR,DBUS_SESSION_BUS_ADDRESS,WAYLAND_DISPLAY,PULSE_SERVER,PULSE_COOKIE qd2 connect ...\n  \
or run QD2 as your regular user after granting access to the QEMU D-Bus socket."
    ))
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum AudioBackend {
    PwPlay,
    Aplay,
}

impl AudioBackend {
    fn name(self) -> &'static str {
        match self {
            Self::PwPlay => "pw-play",
            Self::Aplay => "aplay",
        }
    }

    fn command(self, info: &PCMInfo, stream_label: &str) -> Result<Command> {
        let mut command = Command::new(self.name());
        match self {
            Self::PwPlay => {
                let format =
                    pw_play_format(info).context("`pw-play` does not support this PCM format")?;
                command
                    .arg("--raw")
                    .arg("--rate")
                    .arg(info.freq.to_string())
                    .arg("--channels")
                    .arg(info.nchannels.to_string())
                    .arg("--format")
                    .arg(format)
                    .arg("--media-role")
                    .arg("Game")
                    .arg("-");
            }
            Self::Aplay => {
                let format =
                    aplay_format(info).context("`aplay` does not support this PCM format")?;
                command
                    .arg("-q")
                    .arg("-t")
                    .arg("raw")
                    .arg("-r")
                    .arg(info.freq.to_string())
                    .arg("-c")
                    .arg(info.nchannels.to_string())
                    .arg("-f")
                    .arg(format);
            }
        }

        command.env("PIPEWIRE_LATENCY", "256/48000");
        command.env("PULSE_PROP_media.role", "game");
        command.env("PULSE_PROP_application.name", "QD2");
        command.env("PULSE_PROP_media.name", stream_label);
        Ok(command)
    }
}

fn preferred_backends(info: &PCMInfo) -> Vec<AudioBackend> {
    let mut backends = Vec::new();
    if pw_play_format(info).is_some() {
        backends.push(AudioBackend::PwPlay);
    }
    if aplay_format(info).is_some() {
        backends.push(AudioBackend::Aplay);
    }
    backends
}

fn pw_play_format(info: &PCMInfo) -> Option<&'static str> {
    if info.be != cfg!(target_endian = "big") && info.bits > 8 {
        return None;
    }

    match (info.is_float, info.is_signed, info.bits) {
        (false, false, 8) => Some("u8"),
        (false, true, 8) => Some("s8"),
        (false, true, 16) => Some("s16"),
        (false, true, 32) => Some("s32"),
        (true, _, 32) => Some("f32"),
        (true, _, 64) => Some("f64"),
        _ => None,
    }
}

fn aplay_format(info: &PCMInfo) -> Option<String> {
    let bytes_per_sample = bytes_per_sample(info)?;
    let endian = if info.be { "BE" } else { "LE" };

    match (info.is_float, info.is_signed, info.bits, bytes_per_sample) {
        (false, false, 8, 1) => Some("U8".to_owned()),
        (false, true, 8, 1) => Some("S8".to_owned()),
        (false, signed, 16, 2) => Some(format!("{}16_{endian}", signed_prefix(signed))),
        (false, signed, 24, 3) => Some(format!("{}24_3{endian}", signed_prefix(signed))),
        (false, signed, 24, 4) => Some(format!("{}24_{endian}", signed_prefix(signed))),
        (false, signed, 32, 4) => Some(format!("{}32_{endian}", signed_prefix(signed))),
        (true, _, 32, 4) => Some(format!("FLOAT_{endian}")),
        (true, _, 64, 8) => Some(format!("FLOAT64_{endian}")),
        _ => None,
    }
}

fn signed_prefix(is_signed: bool) -> &'static str {
    if is_signed { "S" } else { "U" }
}

fn bytes_per_sample(info: &PCMInfo) -> Option<usize> {
    let channels = usize::from(info.nchannels);
    if channels == 0 {
        return None;
    }

    let bytes_per_frame = usize::try_from(info.bytes_per_frame).ok()?;
    if bytes_per_frame % channels != 0 {
        return None;
    }

    Some(bytes_per_frame / channels)
}

fn describe_pcm_info(info: &PCMInfo) -> String {
    format!(
        "{}-bit {}{} {} Hz, {} channel(s)",
        info.bits,
        if info.is_float {
            "float"
        } else if info.is_signed {
            "signed"
        } else {
            "unsigned"
        },
        if info.bits > 8 {
            if info.be { " BE" } else { " LE" }
        } else {
            ""
        },
        info.freq,
        info.nchannels
    )
}

fn apply_volume(info: &PCMInfo, volume: &Volume, mut data: Vec<u8>) -> Vec<u8> {
    let bytes_per_sample = match bytes_per_sample(info) {
        Some(bytes) if bytes > 0 => bytes,
        _ => return data,
    };

    if volume.mute {
        fill_with_silence(info, bytes_per_sample, &mut data);
        return data;
    }

    if volume.volume.is_empty() || volume.volume.iter().all(|level| *level == u8::MAX) {
        return data;
    }

    let channels = usize::from(info.nchannels);
    let frame_bytes = usize::try_from(info.bytes_per_frame).unwrap_or(0);
    if frame_bytes == 0 || channels == 0 {
        return data;
    }

    for frame in data.chunks_exact_mut(frame_bytes) {
        for channel in 0..channels {
            let start = channel * bytes_per_sample;
            let end = start + bytes_per_sample;
            let factor = channel_volume(volume, channel);
            scale_sample(info, &mut frame[start..end], factor);
        }
    }

    data
}

fn channel_volume(volume: &Volume, channel: usize) -> f64 {
    volume
        .volume
        .get(channel)
        .copied()
        .or_else(|| volume.volume.last().copied())
        .unwrap_or(u8::MAX) as f64
        / f64::from(u8::MAX)
}

fn fill_with_silence(info: &PCMInfo, bytes_per_sample: usize, data: &mut [u8]) {
    let channels = usize::from(info.nchannels);
    let frame_bytes = usize::try_from(info.bytes_per_frame).unwrap_or(0);
    if frame_bytes == 0 || channels == 0 {
        return;
    }

    for frame in data.chunks_exact_mut(frame_bytes) {
        for channel in 0..channels {
            let start = channel * bytes_per_sample;
            let end = start + bytes_per_sample;
            silence_sample(info, &mut frame[start..end]);
        }
    }
}

fn silence_sample(info: &PCMInfo, sample: &mut [u8]) {
    if info.is_float || info.is_signed {
        sample.fill(0);
        return;
    }

    match sample.len() {
        1 => sample[0] = 0x80,
        2 => {
            let midpoint = if info.be {
                0x8000u16.to_be_bytes()
            } else {
                0x8000u16.to_le_bytes()
            };
            sample.copy_from_slice(&midpoint);
        }
        3 => {
            if info.be {
                sample.copy_from_slice(&[0x80, 0x00, 0x00]);
            } else {
                sample.copy_from_slice(&[0x00, 0x00, 0x80]);
            }
        }
        4 => {
            let midpoint = if info.be {
                0x8000_0000u32.to_be_bytes()
            } else {
                0x8000_0000u32.to_le_bytes()
            };
            sample.copy_from_slice(&midpoint);
        }
        _ => sample.fill(0),
    }
}

fn scale_sample(info: &PCMInfo, sample: &mut [u8], factor: f64) {
    match (info.is_float, info.is_signed, sample.len(), info.bits) {
        (false, true, 1, 8) => {
            let value = i8::from_ne_bytes([sample[0]]);
            sample.copy_from_slice(&(scale_signed(value.into(), factor) as i8).to_ne_bytes());
        }
        (false, false, 1, 8) => sample[0] = scale_unsigned_u8(sample[0], factor),
        (false, true, 2, 16) => scale_signed_i16(sample, factor, info.be),
        (false, false, 2, 16) => scale_unsigned_u16(sample, factor, info.be),
        (false, true, 4, 32) => scale_signed_i32(sample, factor, info.be),
        (false, false, 4, 32) => scale_unsigned_u32(sample, factor, info.be),
        (true, _, 4, 32) => scale_float32(sample, factor, info.be),
        (true, _, 8, 64) => scale_float64(sample, factor, info.be),
        _ => {}
    }
}

fn scale_signed(value: i64, factor: f64) -> i64 {
    (value as f64 * factor).round() as i64
}

fn scale_unsigned_u8(value: u8, factor: f64) -> u8 {
    (((f64::from(value) - 128.0) * factor) + 128.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

fn scale_signed_i16(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        i16::from_be_bytes([sample[0], sample[1]])
    } else {
        i16::from_le_bytes([sample[0], sample[1]])
    };
    let scaled = (f64::from(value) * factor)
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

fn scale_unsigned_u16(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        u16::from_be_bytes([sample[0], sample[1]])
    } else {
        u16::from_le_bytes([sample[0], sample[1]])
    };
    let scaled = ((f64::from(value) - 32768.0) * factor + 32768.0)
        .round()
        .clamp(0.0, f64::from(u16::MAX)) as u16;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

fn scale_signed_i32(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        i32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]])
    } else {
        i32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])
    };
    let scaled = (f64::from(value) * factor)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

fn scale_unsigned_u32(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        u32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]])
    } else {
        u32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])
    };
    let scaled = (((value as f64) - 2_147_483_648.0) * factor + 2_147_483_648.0)
        .round()
        .clamp(0.0, u32::MAX as f64) as u32;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

fn scale_float32(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        f32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]])
    } else {
        f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])
    };
    let scaled = (f64::from(value) * factor) as f32;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

fn scale_float64(sample: &mut [u8], factor: f64, be: bool) {
    let value = if be {
        f64::from_be_bytes([
            sample[0], sample[1], sample[2], sample[3], sample[4], sample[5], sample[6], sample[7],
        ])
    } else {
        f64::from_le_bytes([
            sample[0], sample[1], sample[2], sample[3], sample[4], sample[5], sample[6], sample[7],
        ])
    };
    let scaled = value * factor;
    let bytes = if be {
        scaled.to_be_bytes()
    } else {
        scaled.to_le_bytes()
    };
    sample.copy_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use qemu_display::{PCMInfo, Volume};

    use super::{
        aplay_format, apply_volume, pipewire_sudo_environment_hint_from, preferred_backends,
        pw_play_format,
    };

    fn pcm_info(bits: u8, is_signed: bool, is_float: bool, bytes_per_frame: u32) -> PCMInfo {
        PCMInfo {
            bits,
            is_signed,
            is_float,
            freq: 48_000,
            nchannels: 2,
            bytes_per_frame,
            bytes_per_second: 96_000,
            be: cfg!(target_endian = "big"),
        }
    }

    #[test]
    fn native_s16_prefers_pw_play_and_aplay() {
        let info = pcm_info(16, true, false, 4);
        let backends = preferred_backends(&info);

        assert_eq!(backends.len(), 2);
        assert_eq!(pw_play_format(&info), Some("s16"));
    }

    #[test]
    fn packed_24_bit_uses_aplay_only() {
        let mut info = pcm_info(24, true, false, 6);
        info.be = false;

        assert_eq!(pw_play_format(&info), None);
        assert_eq!(aplay_format(&info).as_deref(), Some("S24_3LE"));
    }

    #[test]
    fn muting_unsigned_audio_writes_midpoint_silence() {
        let info = pcm_info(8, false, false, 2);
        let volume = Volume {
            mute: true,
            volume: vec![255, 255],
        };

        assert_eq!(
            apply_volume(&info, &volume, vec![0x00, 0xff]),
            vec![0x80, 0x80]
        );
    }

    #[test]
    fn signed_16_volume_is_scaled_per_channel() {
        let info = pcm_info(16, true, false, 4);
        let volume = Volume {
            mute: false,
            volume: vec![128, 255],
        };
        let left = 1_000i16.to_le_bytes();
        let right = 2_000i16.to_le_bytes();
        let input = vec![left[0], left[1], right[0], right[1]];

        let output = apply_volume(&info, &volume, input);
        let scaled_left = i16::from_le_bytes([output[0], output[1]]);
        let scaled_right = i16::from_le_bytes([output[2], output[3]]);

        assert!(scaled_left < 1_000);
        assert!(scaled_left > 400);
        assert_eq!(scaled_right, 2_000);
    }

    #[test]
    fn sudo_pipewire_hint_is_suppressed_with_preserved_user_session_env() {
        assert_eq!(
            pipewire_sudo_environment_hint_from(
                Some("alice"),
                Some("1000"),
                Some("/run/user/1000"),
                Some("unix:path=/run/user/1000/bus"),
            ),
            None
        );
    }

    #[test]
    fn sudo_pipewire_hint_is_emitted_when_root_session_env_is_used() {
        let hint = pipewire_sudo_environment_hint_from(
            Some("alice"),
            Some("1000"),
            Some("/run/user/0"),
            None,
        )
        .expect("expected a sudo + PipeWire hint");

        assert!(hint.contains("alice"));
        assert!(hint.contains("--preserve-env"));
        assert!(hint.contains("QEMU D-Bus socket"));
    }
}
