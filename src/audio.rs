// Voice-message capture + in-memory playback.
//
// Recording uses cpal to read the default input device and hound to encode
// mono 16-bit PCM WAV bytes. Playback uses rodio so decrypted bytes never
// touch disk. Both paths are best-effort: any failure logs and surfaces a
// simple error to the caller.
//
// cpal's Stream and rodio's OutputStream are !Send, so AudioRecorder and
// AudioPlayer are confined to the Slint UI thread and stored in thread-locals
// by the caller. Only the rodio Sink is shared with a background monitor
// thread (it is Send + Sync in rodio 0.19).

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::Source;

// ─── Recording ─────────────────────────────────────────────────────────────

const RECORD_SAMPLE_RATE: u32 = 16000;

struct RecordState {
    samples: Vec<f32>,
    channels: u16,
}

/// A live voice recording. Dropping it without calling [`Self::stop`] stops
/// the stream and discards the captured audio.
pub struct AudioRecorder {
    _stream: cpal::Stream,
    state: Arc<Mutex<RecordState>>,
    sample_rate: u32,
}

impl AudioRecorder {
    /// Start capturing from the default input device. Returns an error if no
    /// microphone is available or the device rejects our config.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no audio input device found"))?;
        let config = device
            .default_input_config()
            .map_err(|e| anyhow!("input config: {e}"))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let state = Arc::new(Mutex::new(RecordState {
            samples: Vec::new(),
            channels,
        }));

        let state_c = state.clone();
        let err_fn = |e| eprintln!("[audio] capture error: {e}");

        let stream = if config.sample_format() == cpal::SampleFormat::F32 {
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if let Ok(mut s) = state_c.lock() {
                        s.samples.extend_from_slice(data);
                    }
                },
                err_fn,
                None,
            )
        } else {
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if let Ok(mut s) = state_c.lock() {
                        s.samples
                            .extend(data.iter().map(|v| *v as f32 / i16::MAX as f32));
                    }
                },
                err_fn,
                None,
            )
        }
        .map_err(|e| anyhow!("build input stream: {e}"))?;

        stream.play().map_err(|e| anyhow!("start capture: {e}"))?;

        Ok(AudioRecorder {
            _stream: stream,
            state,
            sample_rate,
        })
    }

    /// Stop capturing and encode the audio as a mono 16-bit PCM WAV file in
    /// memory. The returned bytes are suitable for upload via the encrypted
    /// MIP-04 attachment path.
    pub fn stop(self) -> Result<Vec<u8>> {
        // Dropping `_stream` stops it.
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("record state poisoned"))?;
        let mono = if state.channels == 1 {
            state.samples.clone()
        } else {
            // Mix all channels to mono.
            let ch = state.channels as usize;
            state
                .samples
                .chunks(ch)
                .map(|chunk| chunk.iter().sum::<f32>() / ch as f32)
                .collect()
        };
        let resampled = if self.sample_rate != RECORD_SAMPLE_RATE {
            resample_linear(&mono, self.sample_rate, RECORD_SAMPLE_RATE)
        } else {
            mono
        };
        encode_wav(&resampled, RECORD_SAMPLE_RATE)
    }
}

fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    if input.is_empty() || from_hz == 0 {
        return Vec::new();
    }
    let ratio = to_hz as f64 / from_hz as f64;
    let out_len = (input.len() as f64 * ratio) as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let t = (src - i0 as f64) as f32;
            input[i0] * (1.0 - t) + input[i1] * t
        })
        .collect()
}

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::new(Cursor::new(&mut out), spec).context("create wav writer")?;
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let i16_sample = (clamped * i16::MAX as f32) as i16;
            writer.write_sample(i16_sample).context("write sample")?;
        }
        writer.finalize().context("finalize wav")?;
    }
    Ok(out)
}

// ─── Playback ───────────────────────────────────────────────────────────────

/// Shared playback state updated by the audio thread and read by Rust-side UI
/// callbacks. Times are in seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackState {
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub finished: bool,
}

/// A rodio-backed player for one in-memory audio clip. Created and driven from
/// the Slint UI thread (the underlying OutputStream is !Send). The [`Sink`] is
/// shared with a background monitor thread via an [`Arc<Mutex<Sink>>`].
pub struct AudioPlayer {
    _stream: rodio::OutputStream,
    sink: Arc<Mutex<rodio::Sink>>,
    duration: Duration,
    shared: Arc<Mutex<PlaybackState>>,
    shutdown: Arc<AtomicBool>,
}

impl AudioPlayer {
    /// Decode `bytes` and start playback immediately.
    pub fn play(bytes: Vec<u8>) -> Result<Self> {
        let (_stream, handle) =
            rodio::OutputStream::try_default().map_err(|e| anyhow!("audio output: {e}"))?;
        let sink = rodio::Sink::try_new(&handle).map_err(|e| anyhow!("audio sink: {e}"))?;

        let cursor = Cursor::new(bytes);
        let source = rodio::Decoder::new(cursor).map_err(|e| anyhow!("decode audio: {e}"))?;
        let duration = source.total_duration().unwrap_or_default();
        let shared = Arc::new(Mutex::new(PlaybackState {
            playing: true,
            position: 0.0,
            duration: duration.as_secs_f64(),
            finished: false,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));

        sink.append(source);
        sink.play();

        Ok(AudioPlayer {
            _stream,
            sink: Arc::new(Mutex::new(sink)),
            duration,
            shared,
            shutdown,
        })
    }

    /// Spawn a thread that polls this player and invokes `on_change` whenever
    /// the state meaningfully changes. The thread stops when the player is
    /// dropped or playback finishes.
    pub fn spawn_monitor(&self, mut on_change: impl FnMut(PlaybackState) + Send + 'static) {
        let shared = self.shared.clone();
        let sink = self.sink.clone();
        let shutdown = self.shutdown.clone();
        std::thread::spawn(move || {
            let mut last = PlaybackState::default();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let current = {
                    let guard = sink.lock().unwrap();
                    let pos = guard.get_pos().as_secs_f64();
                    let finished = guard.empty();
                    let mut s = shared.lock().unwrap();
                    s.position = pos;
                    s.playing = !guard.is_paused() && !finished;
                    s.finished = finished;
                    *s
                };
                if current != last {
                    on_change(current);
                    last = current;
                }
                if current.finished {
                    break;
                }
            }
        });
    }

    pub fn toggle(&self) {
        let guard = self.sink.lock().unwrap();
        if guard.is_paused() {
            guard.play();
        } else {
            guard.pause();
        }
    }

    pub fn seek(&self, secs: f64) {
        let target = secs.clamp(0.0, self.duration.as_secs_f64());
        let _ = self
            .sink
            .lock()
            .unwrap()
            .try_seek(Duration::from_secs_f64(target));
    }

    pub fn state(&self) -> PlaybackState {
        self.shared.lock().map(|s| *s).unwrap_or_default()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.sink.lock().unwrap().stop();
    }
}

impl PartialEq for PlaybackState {
    fn eq(&self, other: &Self) -> bool {
        self.playing == other.playing
            && self.finished == other.finished
            && (self.position - other.position).abs() < 0.05
            && (self.duration - other.duration).abs() < 0.05
    }
}

impl Eq for PlaybackState {}
