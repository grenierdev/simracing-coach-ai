//! Microphone capture for push-to-talk.
//!
//! The input stream stays open at all times; a ring buffer keeps the last
//! 300 milliseconds so the first syllable is not cut when the driver starts
//! talking as they press the button.

use std::collections::VecDeque;
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

pub struct Recorder {
    capture: Arc<Mutex<Capture>>,
    device: String,
    _thread: std::thread::JoinHandle<()>,
}

impl Recorder {
    /// Opens the default microphone.
    pub fn open() -> Result<Self, VoiceError> {
        let capture: Arc<Mutex<Capture>> = Arc::default();
        let (tx, rx) = crossbeam_channel::bounded::<Result<String, String>>(1);
        let c = capture.clone();
        let thread = std::thread::Builder::new()
            .name("audio-in".into())
            .spawn(move || {
                let stream = match open_stream(c) {
                    Ok((s, name)) => {
                        let _ = tx.send(Ok(name));
                        s
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                loop {
                    std::thread::park();
                    let _ = &stream;
                }
            })
            .map_err(|e| VoiceError::Output(e.to_string()))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(device)) => Ok(Recorder {
                capture,
                device,
                _thread: thread,
            }),
            Ok(Err(e)) => Err(VoiceError::Output(format!("microphone: {e}"))),
            Err(_) => Err(VoiceError::Output(
                "the microphone is not responding".into(),
            )),
        }
    }

    pub fn device(&self) -> &str {
        &self.device
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

fn open_stream(capture: Arc<Mutex<Capture>>) -> Result<(cpal::Stream, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SizedSample};

    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("no microphone")?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "default microphone".into());
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    *capture.lock().unwrap() = Capture::new(config.sample_rate);

    fn build<T: SizedSample + Into<MicSample>>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        channels: usize,
        capture: Arc<Mutex<Capture>>,
    ) -> Result<cpal::Stream, String> {
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|f| f.iter().map(|s| (*s).into().0).sum::<f32>() / channels as f32)
                        .collect();
                    capture.lock().unwrap().push(&mono);
                },
                |e| tracing::warn!("microphone: {e}"),
                None,
            )
            .map_err(|e| e.to_string())
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, channels, capture),
        SampleFormat::I16 => build::<i16>(&device, config, channels, capture),
        SampleFormat::U16 => build::<u16>(&device, config, channels, capture),
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
