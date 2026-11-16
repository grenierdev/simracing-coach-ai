//! Microphone capture for push-to-talk.
//!
//! The input stream stays open at all times; a ring buffer keeps the last
//! 300 milliseconds so the first syllable is not cut when the driver starts
//! talking as they press the button.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Audio, VoiceError};

const PREROLL_S: f64 = 0.3;
const MAX_S: f64 = 30.0;

#[derive(Default)]
pub struct Capture {
    recording: bool,
    preroll: VecDeque<f32>,
    buf: Vec<f32>,
    rate: u32,
    /// RMS level of the last block, 0..1.
    level: f32,
}

impl Capture {
    pub fn new(rate: u32) -> Self {
        Capture {
            rate,
            ..Default::default()
        }
    }

    /// Adds mono samples.
    pub fn push(&mut self, mono: &[f32]) {
        if !mono.is_empty() {
            let rms = (mono.iter().map(|x| x * x).sum::<f32>() / mono.len() as f32).sqrt();
            self.level = rms.min(1.0);
        }
        if self.recording {
            let max = (MAX_S * self.rate as f64) as usize;
            let room = max.saturating_sub(self.buf.len());
            self.buf.extend_from_slice(&mono[..mono.len().min(room)]);
        } else {
            let keep = (PREROLL_S * self.rate as f64) as usize;
            self.preroll.extend(mono.iter().copied());
            while self.preroll.len() > keep {
                self.preroll.pop_front();
            }
        }
    }

    pub fn start(&mut self) {
        self.buf.clear();
        self.buf.extend(self.preroll.drain(..));
        self.recording = true;
    }

    pub fn stop(&mut self) -> Audio {
        self.recording = false;
        Audio {
            samples: std::mem::take(&mut self.buf),
            sample_rate: self.rate,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    pub fn level(&self) -> f32 {
        self.level
    }
}

/// Push-to-talk recording source (real microphone, or a fake in tests).
pub trait Microphone: Send + Sync {
    fn start(&self);
    fn stop(&self) -> Audio;
    fn level(&self) -> f32;
}

impl Microphone for Recorder {
    fn start(&self) {
        Recorder::start(self)
    }
    fn stop(&self) -> Audio {
        Recorder::stop(self)
    }
    fn level(&self) -> f32 {
        Recorder::level(self)
    }
}

/// Microphone with a switchable device. Without a device it records silence.
pub struct Recorder {
    capture: Arc<Mutex<Capture>>,
    stream: Mutex<Option<StreamThread>>,
    /// Generation of the active stream; callbacks of older streams are ignored.
    generation: Arc<AtomicU64>,
}

/// Thread holding the cpal stream (not `Send` on every platform); dropping
/// `_stop` ends the thread, which closes the stream.
struct StreamThread {
    device: String,
    _stop: crossbeam_channel::Sender<()>,
}

/// Where a stream callback writes.
struct Target {
    capture: Arc<Mutex<Capture>>,
    current: Arc<AtomicU64>,
    generation: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    /// Recorder without a device: call [`Recorder::select`].
    pub fn new() -> Self {
        Recorder {
            capture: Arc::default(),
            stream: Mutex::new(None),
            generation: Arc::default(),
        }
    }

    /// Opens the default microphone.
    pub fn open() -> Result<Self, VoiceError> {
        let r = Self::new();
        r.select(None)?;
        Ok(r)
    }

    /// Switches to the device `id` (`None`: system default). On failure the
    /// previous device stays open. Returns the device name.
    pub fn select(&self, id: Option<&str>) -> Result<String, VoiceError> {
        let mut stream = self.stream.lock().unwrap();
        let previous = self.generation.load(Ordering::SeqCst);
        let generation = previous + 1;
        let id = id.map(str::to_string);
        let (tx, rx) = crossbeam_channel::bounded::<Result<String, String>>(1);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let target = Target {
            capture: self.capture.clone(),
            current: self.generation.clone(),
            generation,
        };
        std::thread::Builder::new()
            .name("audio-in".into())
            .spawn(move || {
                let s = match open_stream(id.as_deref(), target) {
                    Ok((s, name)) => {
                        let _ = tx.send(Ok(name));
                        s
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let _ = stop_rx.recv();
                drop(s);
            })
            .map_err(|e| VoiceError::Input(e.to_string()))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(device)) => {
                // From here on, only the new stream's callbacks are recorded;
                // replacing the previous thread's sender closes its stream.
                self.generation.store(generation, Ordering::SeqCst);
                *stream = Some(StreamThread {
                    device: device.clone(),
                    _stop: stop_tx,
                });
                Ok(device)
            }
            Ok(Err(e)) => Err(VoiceError::Input(e)),
            Err(_) => Err(VoiceError::Input("not responding".into())),
        }
    }

    /// Name of the open device, if any.
    pub fn device(&self) -> Option<String> {
        self.stream
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.device.clone())
    }

    pub fn start(&self) {
        self.capture.lock().unwrap().start();
    }

    pub fn stop(&self) -> Audio {
        self.capture.lock().unwrap().stop()
    }

    pub fn is_recording(&self) -> bool {
        self.capture.lock().unwrap().is_recording()
    }

    pub fn level(&self) -> f32 {
        self.capture.lock().unwrap().level()
    }
}

fn open_stream(id: Option<&str>, target: Target) -> Result<(cpal::Stream, String), String> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use cpal::{SampleFormat, SizedSample};

    let device = crate::devices::find(crate::Direction::Input, id)?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "default microphone".into());
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let rate = config.sample_rate;

    fn build<T: SizedSample + Into<MicSample>>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        channels: usize,
        rate: u32,
        target: Target,
    ) -> Result<cpal::Stream, String> {
        let mut active = false;
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    if target.current.load(Ordering::Relaxed) != target.generation {
                        return;
                    }
                    let mut c = target.capture.lock().unwrap();
                    if !active {
                        // First block of this stream: reset to its sample rate.
                        *c = Capture::new(rate);
                        active = true;
                    }
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|f| f.iter().map(|s| (*s).into().0).sum::<f32>() / channels as f32)
                        .collect();
                    c.push(&mono);
                },
                |e| tracing::warn!("microphone: {e}"),
                None,
            )
            .map_err(|e| e.to_string())
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, channels, rate, target),
        SampleFormat::I16 => build::<i16>(&device, config, channels, rate, target),
        SampleFormat::U16 => build::<u16>(&device, config, channels, rate, target),
        other => Err(format!("unsupported sample format: {other}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, name))
}

/// Converts the microphone sample formats to float -1..1.
pub struct MicSample(f32);

impl From<f32> for MicSample {
    fn from(v: f32) -> Self {
        MicSample(v)
    }
}
impl From<i16> for MicSample {
    fn from(v: i16) -> Self {
        MicSample(v as f32 / 32_768.0)
    }
}
impl From<u16> for MicSample {
    fn from(v: u16) -> Self {
        MicSample((v as f32 - 32_768.0) / 32_768.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preroll_is_kept_and_recording_bounded() {
        let mut c = Capture::new(1000);
        c.push(&[0.1; 500]); // 0.5 s before the press: only 0.3 s is kept
        c.start();
        c.push(&[0.2; 1000]);
        let a = c.stop();
        assert_eq!(a.samples.len(), 300 + 1000);
        assert_eq!(a.samples[0], 0.1);
        assert_eq!(a.samples[300], 0.2);
        assert!((c.level() - 0.2).abs() < 1e-3);
        // 30 s cap.
        c.start();
        for _ in 0..40 {
            c.push(&[0.0; 1000]);
        }
        assert_eq!(c.stop().samples.len(), 30_000);
    }
}
