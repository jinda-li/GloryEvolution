use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
};
use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};

use crate::{AppError, AppResult};

/// Speech models are trained on 16 kHz mono; sending that instead of the
/// device's native 48 kHz stereo cuts the upload ~6x.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Takes up to this long are sent as one request; longer ones are split so no
/// single upload or transcript hits provider size, time or output-token limits.
const MAX_SEGMENT_SECONDS: usize = 60;
/// Cuts are placed at the quietest moment between these offsets into a segment.
const MIN_CUT_SECONDS: usize = 35;
const MAX_CUT_SECONDS: usize = 55;
/// How often the capture thread checks whether the stream died.
const STREAM_POLL: Duration = Duration::from_millis(250);

pub type LevelCallback = Arc<dyn Fn(f32) + Send + Sync + 'static>;

/// Lifecycle notices from the capture thread, for surfacing in the UI.
#[derive(Debug, Clone, Copy)]
pub enum CaptureNotice {
    /// The input stream errored (device unplugged, Bluetooth profile switch, ...).
    Interrupted,
    /// A replacement stream is running and samples are flowing again.
    Recovered,
}

pub type NoticeCallback = Arc<dyn Fn(CaptureNotice) + Send + Sync + 'static>;

/// Owns the audio thread. `cpal::Stream` is not `Send` on every platform, so
/// it lives and dies on its own thread and we talk to it over a channel.
pub struct RecordingSession {
    stop_tx: Sender<()>,
    handle: JoinHandle<Captured>,
    started_at: Instant,
}

struct Captured {
    samples: Vec<i16>,
    peak: f32,
}

pub struct StoppedRecording {
    pub samples: Vec<i16>,
    pub duration_seconds: f64,
    pub peak: f32,
}

impl RecordingSession {
    pub fn elapsed_seconds(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    pub fn stop(self) -> AppResult<StoppedRecording> {
        let _ = self.stop_tx.send(());
        let captured = self
            .handle
            .join()
            .map_err(|_| AppError::Audio("录音线程异常退出。".to_string()))?;
        let duration_seconds = captured.samples.len() as f64 / TARGET_SAMPLE_RATE as f64;
        Ok(StoppedRecording {
            samples: captured.samples,
            duration_seconds,
            peak: captured.peak,
        })
    }
}

pub fn list_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default()
}

pub fn start(
    device_name: &str,
    level_callback: LevelCallback,
    notice_callback: NoticeCallback,
) -> AppResult<RecordingSession> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<AppResult<()>>();
    let device_name = device_name.trim().to_string();

    let handle = std::thread::Builder::new()
        .name("audio-capture".to_string())
        .spawn(move || {
            run_capture(&device_name, level_callback, notice_callback, ready_tx, stop_rx)
        })
        .map_err(|error| AppError::Audio(format!("无法启动录音线程：{error}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RecordingSession {
            stop_tx,
            handle,
            started_at: Instant::now(),
        }),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(error)
        }
        Err(_) => {
            let _ = handle.join();
            Err(AppError::Audio("录音线程启动失败。".to_string()))
        }
    }
}

fn run_capture(
    device_name: &str,
    level_callback: LevelCallback,
    notice_callback: NoticeCallback,
    ready_tx: Sender<AppResult<()>>,
    stop_rx: Receiver<()>,
) -> Captured {
    let shared = Arc::new(Mutex::new(Captured {
        samples: Vec::with_capacity(TARGET_SAMPLE_RATE as usize * 60),
        peak: 0.0,
    }));
    let failed = Arc::new(AtomicBool::new(false));

    let mut stream = match play_stream(device_name, &shared, &level_callback, &failed) {
        Ok(stream) => Some(stream),
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return take(&shared);
        }
    };
    let _ = ready_tx.send(Ok(()));

    // A dead stream used to end capture silently, so a long take kept only
    // the audio before the glitch. Reopen it instead and keep appending.
    loop {
        match stop_rx.recv_timeout(STREAM_POLL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !failed.swap(false, Ordering::SeqCst) {
            continue;
        }
        if stream.take().is_some() {
            notice_callback(CaptureNotice::Interrupted);
        }
        match play_stream(device_name, &shared, &level_callback, &failed) {
            Ok(replacement) => {
                stream = Some(replacement);
                notice_callback(CaptureNotice::Recovered);
            }
            // Retry on the next poll; the device may still be re-enumerating.
            Err(_) => failed.store(true, Ordering::SeqCst),
        }
    }

    drop(stream);
    take(&shared)
}

fn play_stream(
    device_name: &str,
    shared: &Arc<Mutex<Captured>>,
    level_callback: &LevelCallback,
    failed: &Arc<AtomicBool>,
) -> AppResult<cpal::Stream> {
    let stream = open_stream(device_name, shared.clone(), level_callback.clone(), failed.clone())?;
    stream.play()?;
    Ok(stream)
}

fn take(shared: &Arc<Mutex<Captured>>) -> Captured {
    let mut guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Captured {
        samples: std::mem::take(&mut guard.samples),
        peak: guard.peak,
    }
}

fn open_stream(
    device_name: &str,
    shared: Arc<Mutex<Captured>>,
    level_callback: LevelCallback,
    failed: Arc<AtomicBool>,
) -> AppResult<cpal::Stream> {
    let host = cpal::default_host();
    let device = if device_name.is_empty() {
        None
    } else {
        host.input_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| d.name().ok().as_deref() == Some(device_name)))
    };
    let device = match device.or_else(|| host.default_input_device()) {
        Some(device) => device,
        None => {
            return Err(AppError::Audio(
                "没有找到可用的麦克风，请检查设备连接和系统麦克风权限。".to_string(),
            ))
        }
    };

    let supported_config = device.default_input_config()?;
    let sample_format = supported_config.sample_format();
    let config: StreamConfig = supported_config.into();

    macro_rules! build {
        ($t:ty) => {
            build_stream::<$t>(&device, &config, shared, level_callback, failed)
        };
    }

    match sample_format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        sample => Err(AppError::Audio(format!(
            "暂不支持当前麦克风采样格式：{sample:?}"
        ))),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<Mutex<Captured>>,
    level_callback: LevelCallback,
    failed: Arc<AtomicBool>,
) -> AppResult<cpal::Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate.0, TARGET_SAMPLE_RATE);
    let mut out = Vec::with_capacity(4096);

    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut sum_squares = 0.0f32;
            let mut frames = 0usize;
            out.clear();

            for frame in data.chunks(channels) {
                let mono = frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>()
                    / frame.len() as f32;
                let mono = mono.clamp(-1.0, 1.0);
                sum_squares += mono * mono;
                frames += 1;
                resampler.push(mono, &mut out);
            }

            if frames == 0 {
                return;
            }
            let rms = (sum_squares / frames as f32).sqrt();
            if let Ok(mut guard) = shared.lock() {
                guard
                    .samples
                    .extend(out.iter().map(|v| (v * i16::MAX as f32) as i16));
                guard.peak = guard.peak.max(rms);
            }
            level_callback((rms * 5.0).clamp(0.0, 1.0));
        },
        move |error| {
            eprintln!("audio input stream error: {error}");
            failed.store(true, Ordering::SeqCst);
        },
        None,
    )?;

    Ok(stream)
}

/// Linear-interpolating resampler with a one-pole low-pass in front to keep
/// aliasing out of the speech band when downsampling.
struct Resampler {
    step: f64,
    next_t: f64,
    index: u64,
    prev: f32,
    lp_state: f32,
    lp_alpha: f32,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        let in_rate = in_rate.max(1) as f32;
        let cutoff = (out_rate as f32 * 0.45).min(in_rate * 0.45);
        let lp_alpha = if in_rate as u32 > out_rate {
            1.0 - (-2.0 * std::f32::consts::PI * cutoff / in_rate).exp()
        } else {
            1.0
        };
        Self {
            step: in_rate as f64 / out_rate as f64,
            next_t: 0.0,
            index: 0,
            prev: 0.0,
            lp_state: 0.0,
            lp_alpha,
        }
    }

    fn push(&mut self, sample: f32, out: &mut Vec<f32>) {
        self.lp_state += self.lp_alpha * (sample - self.lp_state);
        let x = self.lp_state;
        let n = self.index as f64;
        while self.next_t <= n {
            let frac = (self.next_t - (n - 1.0)) as f32;
            let value = if self.index == 0 {
                x
            } else {
                self.prev + (x - self.prev) * frac
            };
            out.push(value);
            self.next_t += self.step;
        }
        self.prev = x;
        self.index += 1;
    }
}

/// Splits a take into transcription-sized pieces, cutting at pauses so no
/// word is chopped in half.
pub fn split_segments(samples: &[i16]) -> Vec<&[i16]> {
    let rate = TARGET_SAMPLE_RATE as usize;
    let mut segments = Vec::new();
    let mut rest = samples;
    while rest.len() > MAX_SEGMENT_SECONDS * rate {
        let cut = quietest_point(rest, MIN_CUT_SECONDS * rate, MAX_CUT_SECONDS * rate);
        let (head, tail) = rest.split_at(cut);
        segments.push(head);
        rest = tail;
    }
    if !rest.is_empty() || segments.is_empty() {
        segments.push(rest);
    }
    segments
}

/// Centre of the lowest-energy 30 ms window in `samples[from..to]`.
fn quietest_point(samples: &[i16], from: usize, to: usize) -> usize {
    const WINDOW: usize = TARGET_SAMPLE_RATE as usize * 3 / 100;
    const HOP: usize = TARGET_SAMPLE_RATE as usize / 100;
    let to = to.min(samples.len().saturating_sub(WINDOW));
    let mut best = (u64::MAX, to);
    let mut start = from;
    while start < to {
        let energy = samples[start..start + WINDOW]
            .iter()
            .map(|s| (*s as i64 * *s as i64) as u64)
            .sum::<u64>();
        if energy < best.0 {
            best = (energy, start + WINDOW / 2);
        }
        start += HOP;
    }
    best.1
}

pub fn encode_wav(samples: &[i16]) -> AppResult<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: WavSampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::with_capacity(samples.len() * 2 + 44));
    {
        let mut writer = WavWriter::new(&mut cursor, spec)?;
        let mut sample_writer = writer.get_i16_writer(samples.len() as u32);
        for sample in samples {
            sample_writer.write_sample(*sample);
        }
        sample_writer.flush()?;
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_produces_expected_length() {
        let mut resampler = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for i in 0..48_000 {
            resampler.push(((i as f32) * 0.01).sin() * 0.5, &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() <= 1, "len {}", out.len());
    }

    #[test]
    fn short_take_is_one_segment() {
        let samples = vec![1i16; TARGET_SAMPLE_RATE as usize * 30];
        assert_eq!(split_segments(&samples).len(), 1);
        assert_eq!(split_segments(&[]).len(), 1);
    }

    #[test]
    fn long_take_splits_at_pause_without_losing_samples() {
        let rate = TARGET_SAMPLE_RATE as usize;
        let mut samples = vec![8_000i16; rate * 150];
        let pause = rate * 40;
        samples[pause..pause + rate / 2].fill(0);

        let segments = split_segments(&samples);
        assert!(segments.len() >= 3);
        assert!(segments.iter().all(|s| s.len() <= MAX_SEGMENT_SECONDS * rate));
        assert_eq!(segments.iter().map(|s| s.len()).sum::<usize>(), samples.len());
        let first = segments[0].len();
        assert!(first >= pause && first <= pause + rate / 2, "cut at {first}");
    }

    #[test]
    fn encodes_valid_wav_header() {
        let wav = encode_wav(&[0, 1, -1, 100]).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 8);
    }
}
