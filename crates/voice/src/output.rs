//! Audio output.
//!
//! [`CpalSink`] plays on a chosen or the default device (WASAPI on Windows): a thread keeps
//! the cpal stream open and the audio callback pulls from a sample queue.
//! [`NullSink`] simulates real-time playback when no device is available
//! (server, CI), so cue sequencing stays correct.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{Audio, VoiceError};

pub trait AudioSink: Send + Sync {
    fn name(&self) -> String;
    /// Queues the audio; returns immediately.
    fn play(&self, audio: &Audio);
    /// Stops playback (fade of a few milliseconds).
    fn stop(&self);
    fn is_playing(&self) -> bool;
    /// RMS level of what is being played, 0..1 (for a level meter).
    fn level(&self) -> f32 {
        0.0
    }
}

/// Simulated playback: lasts the audio duration without playing anything,
/// and reports the level the audio would have, so meters work without a
/// sound card.
#[derive(Default)]
pub struct NullSink {
    queue: Mutex<VecDeque<(Instant, Arc<Audio>)>>,
}

impl NullSink {
    fn end_of(start: Instant, audio: &Audio) -> Instant {
        start + Duration::from_secs_f64(audio.duration_s())
    }

    fn prune(q: &mut VecDeque<(Instant, Arc<Audio>)>, now: Instant) {
        while q.front().is_some_and(|(t, a)| Self::end_of(*t, a) <= now) {
            q.pop_front();
        }
    }
}

impl AudioSink for NullSink {
    fn name(&self) -> String {
        "no audio output".into()
    }

    fn play(&self, audio: &Audio) {
        let now = Instant::now();
        let mut q = self.queue.lock().unwrap();
        Self::prune(&mut q, now);
        let start = q
            .back()
            .map(|(t, a)| Self::end_of(*t, a))
            .filter(|t| *t > now)
            .unwrap_or(now);
        q.push_back((start, Arc::new(audio.clone())));
    }

    fn stop(&self) {
        self.queue.lock().unwrap().clear();
    }

    fn is_playing(&self) -> bool {
        let mut q = self.queue.lock().unwrap();
        Self::prune(&mut q, Instant::now());
        !q.is_empty()
    }

    fn level(&self) -> f32 {
        let now = Instant::now();
        let mut q = self.queue.lock().unwrap();
        Self::prune(&mut q, now);
        let Some((start, audio)) = q.front().filter(|(t, _)| *t <= now) else {
            return 0.0;
        };
        // RMS over the 30 ms being "played".
        let rate = audio.sample_rate as f64;
        let at = (now.duration_since(*start).as_secs_f64() * rate) as usize;
        let window = &audio.samples
            [at.min(audio.samples.len())..(at + (0.03 * rate) as usize).min(audio.samples.len())];
        if window.is_empty() {
            return 0.0;
        }
        (window.iter().map(|x| x * x).sum::<f32>() / window.len() as f32)
            .sqrt()
            .min(1.0)
    }
}

/// Linear resampling (good enough for voice).
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let n = (samples.len() as f64 / ratio).floor() as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * ratio;
            let j = x.floor() as usize;
            let f = (x - j as f64) as f32;
            let a = samples[j];
            let b = samples.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * f
        })
        .collect()
}

struct Queue {
    samples: VecDeque<f32>,
}

pub struct CpalSink {
    queue: Arc<Mutex<Queue>>,
    /// RMS of the last audio block played (f32 bits).
    level: Arc<AtomicU32>,
    sample_rate: u32,
    device: String,
    /// Dropping it ends the thread holding the stream, which closes it.
    _stop: crossbeam_channel::Sender<()>,
}

impl CpalSink {
    /// Opens the default output device.
    pub fn open() -> Result<Self, VoiceError> {
        Self::open_device(None)
    }

    /// Opens the device `id` (`None`: system default).
    pub fn open_device(id: Option<&str>) -> Result<Self, VoiceError> {
        let id = id.map(str::to_string);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let level = Arc::new(AtomicU32::new(0));
        let lv = level.clone();
        let queue = Arc::new(Mutex::new(Queue {
            samples: VecDeque::new(),
        }));
        let (tx, rx) = crossbeam_channel::bounded::<Result<(u32, String), String>>(1);
        let q = queue.clone();
        // The cpal stream is not `Send` on every platform: it lives and dies in
        // its own thread.
        std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                let stream = match open_stream(id.as_deref(), q, lv) {
                    Ok((stream, rate, name)) => {
                        let _ = tx.send(Ok((rate, name)));
                        stream
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                // Keep the stream open until the sink is dropped.
                let _ = stop_rx.recv();
                drop(stream);
            })
            .map_err(|e| VoiceError::Output(e.to_string()))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok((sample_rate, device))) => Ok(CpalSink {
                queue,
                level,
                sample_rate,
                device,
                _stop: stop_tx,
            }),
            Ok(Err(e)) => Err(VoiceError::Output(e)),
            Err(_) => Err(VoiceError::Output("the device is not responding".into())),
        }
    }
}

fn open_stream(
    id: Option<&str>,
    queue: Arc<Mutex<Queue>>,
    level: Arc<AtomicU32>,
) -> Result<(cpal::Stream, u32, String), String> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    let device = crate::devices::find(crate::Direction::Output, id)?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "default output".into());
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;
    let channels = config.channels as usize;

    fn build<T: SizedSample + FromSample<f32>>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        channels: usize,
        queue: Arc<Mutex<Queue>>,
        level: Arc<AtomicU32>,
    ) -> Result<cpal::Stream, String> {
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    let mut q = queue.lock().unwrap();
                    let (mut sum, mut n) = (0.0f32, 0usize);
                    for frame in data.chunks_mut(channels) {
                        let v = q.samples.pop_front().unwrap_or(0.0);
                        sum += v * v;
                        n += 1;
                        for out in frame {
                            *out = T::from_sample(v);
                        }
                    }
                    let rms = if n > 0 { (sum / n as f32).sqrt() } else { 0.0 };
                    level.store(rms.min(1.0).to_bits(), Ordering::Relaxed);
                },
                |e| tracing::warn!("audio output: {e}"),
                None,
            )
            .map_err(|e| e.to_string())
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, channels, queue, level),
        SampleFormat::I16 => build::<i16>(&device, config, channels, queue, level),
        SampleFormat::I32 => build::<i32>(&device, config, channels, queue, level),
        SampleFormat::U16 => build::<u16>(&device, config, channels, queue, level),
        other => Err(format!("unsupported sample format: {other}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate, name))
}

impl AudioSink for CpalSink {
    fn name(&self) -> String {
        format!("{} ({} Hz)", self.device, self.sample_rate)
    }

    fn play(&self, audio: &Audio) {
        let s = resample(&audio.samples, audio.sample_rate, self.sample_rate);
        self.queue.lock().unwrap().samples.extend(s);
    }

    fn stop(&self) {
        let mut q = self.queue.lock().unwrap();
        // 15 ms fade to avoid a click.
        let n = (self.sample_rate as usize * 15 / 1000).min(q.samples.len());
        let fade: Vec<f32> = q
            .samples
            .iter()
            .take(n)
            .enumerate()
            .map(|(i, v)| v * (1.0 - i as f32 / n.max(1) as f32))
            .collect();
        q.samples.clear();
        q.samples.extend(fade);
    }

    fn is_playing(&self) -> bool {
        !self.queue.lock().unwrap().samples.is_empty()
    }

    fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

/// Output whose device can be changed while running: the speaker keeps the
/// same handle, playback continues on the new device. It also sets the
/// volume of the coach's voice.
pub struct SwitchableSink {
    inner: Mutex<Arc<dyn AudioSink>>,
    /// Gain applied to each sentence (`f32` bits), 1.0 = unchanged.
    gain: AtomicU32,
}

impl SwitchableSink {
    pub fn new(inner: Arc<dyn AudioSink>) -> Self {
        SwitchableSink {
            inner: Mutex::new(inner),
            gain: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    /// Volume of the coach's voice: 0.0 (silent) to 1.5, from the next
    /// sentence on.
    pub fn set_volume(&self, gain: f32) {
        let g = if gain.is_finite() {
            gain.clamp(0.0, 1.5)
        } else {
            1.0
        };
        self.gain.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    /// Replaces the output; the sentence being played on the old one stops.
    pub fn replace(&self, sink: Arc<dyn AudioSink>) {
        let old = std::mem::replace(&mut *self.inner.lock().unwrap(), sink);
        old.stop();
    }

    fn current(&self) -> Arc<dyn AudioSink> {
        self.inner.lock().unwrap().clone()
    }
}

impl AudioSink for SwitchableSink {
    fn name(&self) -> String {
        self.current().name()
    }
    fn play(&self, audio: &Audio) {
        let g = self.volume();
        if (g - 1.0).abs() < 1e-3 {
            return self.current().play(audio);
        }
        let scaled = Audio {
            samples: audio
                .samples
                .iter()
                .map(|s| (s * g).clamp(-1.0, 1.0))
                .collect(),
            sample_rate: audio.sample_rate,
        };
        self.current().play(&scaled)
    }
    fn stop(&self) {
        self.current().stop()
    }
    fn is_playing(&self) -> bool {
        self.current().is_playing()
    }
    fn level(&self) -> f32 {
        self.current().level()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_duration_and_shape() {
        let src: Vec<f32> = (0..22_050).map(|i| i as f32 / 22_050.0).collect();
        let out = resample(&src, 22_050, 48_000);
        assert!((out.len() as i64 - 48_000).abs() <= 3);
        assert!((out[24_000] - 0.5).abs() < 0.001);
    }

    #[test]
    fn switchable_sink_delegates_to_the_current_output() {
        let s = SwitchableSink::new(Arc::new(NullSink::default()));
        let a = Audio {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        };
        s.play(&a);
        assert!(s.is_playing());
        s.replace(Arc::new(NullSink::default()));
        assert!(!s.is_playing(), "the new output starts silent");
        s.play(&a);
        assert!(s.is_playing());
    }

    #[test]
    fn volume_scales_the_next_sentences() {
        let s = SwitchableSink::new(Arc::new(NullSink::default()));
        assert_eq!(s.volume(), 1.0);
        let a = Audio {
            samples: vec![0.5; 16_000],
            sample_rate: 16_000,
        };
        s.set_volume(0.5);
        s.play(&a);
        assert!((s.level() - 0.25).abs() < 1e-3, "{}", s.level());
        s.stop();
        // Louder than the source, clipped at full scale.
        s.set_volume(4.0);
        assert_eq!(s.volume(), 1.5);
        s.play(&Audio {
            samples: vec![0.9; 16_000],
            sample_rate: 16_000,
        });
        assert!((s.level() - 1.0).abs() < 1e-3, "{}", s.level());
    }

    #[test]
    fn null_sink_simulates_playback_time() {
        let s = NullSink::default();
        assert!(!s.is_playing());
        s.play(&Audio {
            samples: vec![0.0; 1600],
            sample_rate: 16_000,
        });
        assert!(s.is_playing());
        std::thread::sleep(Duration::from_millis(150));
        assert!(!s.is_playing());
        // Level of the audio being "played".
        s.play(&Audio {
            samples: vec![0.5; 16_000],
            sample_rate: 16_000,
        });
        assert!((s.level() - 0.5).abs() < 1e-3);
        s.stop();
        assert_eq!(s.level(), 0.0);
        s.play(&Audio {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        });
        s.stop();
        assert!(!s.is_playing());
    }
}
