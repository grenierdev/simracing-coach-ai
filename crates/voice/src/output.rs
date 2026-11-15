//! Sortie audio.
//!
//! [`CpalSink`] joue sur le périphérique par défaut (WASAPI sous Windows) : un
//! thread garde le flux cpal ouvert et le rappel audio puise dans une file
//! d'échantillons. [`NullSink`] simule la lecture en temps réel quand aucun
//! périphérique n'est disponible (serveur, CI) : l'enchaînement des consignes
//! reste correct.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{Audio, VoiceError};

pub trait AudioSink: Send + Sync {
    fn name(&self) -> String;
    /// Met l'audio en file ; retourne immédiatement.
    fn play(&self, audio: &Audio);
    /// Coupe la lecture (fondu de quelques millisecondes).
    fn stop(&self);
    fn is_playing(&self) -> bool;
}

/// Lecture simulée : occupe la durée de l'audio sans rien jouer.
#[derive(Default)]
pub struct NullSink {
    until: Mutex<Option<Instant>>,
}

impl AudioSink for NullSink {
    fn name(&self) -> String {
        "aucune sortie audio".into()
    }

    fn play(&self, audio: &Audio) {
        let mut u = self.until.lock().unwrap();
        let start = u
            .filter(|t| *t > Instant::now())
            .unwrap_or_else(Instant::now);
        *u = Some(start + Duration::from_secs_f64(audio.duration_s()));
    }

    fn stop(&self) {
        *self.until.lock().unwrap() = None;
    }

    fn is_playing(&self) -> bool {
        self.until
            .lock()
            .unwrap()
            .is_some_and(|t| t > Instant::now())
    }
}

/// Rééchantillonnage linéaire (suffisant pour de la voix).
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
    sample_rate: u32,
    device: String,
    _thread: std::thread::JoinHandle<()>,
}

impl CpalSink {
    /// Ouvre le périphérique de sortie par défaut.
    pub fn open() -> Result<Self, VoiceError> {
        let queue = Arc::new(Mutex::new(Queue {
            samples: VecDeque::new(),
        }));
        let (tx, rx) = crossbeam_channel::bounded::<Result<(u32, String), String>>(1);
        let q = queue.clone();
        // Le flux cpal n'est pas `Send` sur toutes les plateformes : il vit et
        // meurt dans son thread.
        let thread = std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                let stream = match open_stream(q) {
                    Ok((stream, rate, name)) => {
                        let _ = tx.send(Ok((rate, name)));
                        stream
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                // Garde le flux ouvert pour la durée du programme.
                loop {
                    std::thread::park();
                    let _ = &stream;
                }
            })
            .map_err(|e| VoiceError::Output(e.to_string()))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok((sample_rate, device))) => Ok(CpalSink {
                queue,
                sample_rate,
                device,
                _thread: thread,
            }),
            Ok(Err(e)) => Err(VoiceError::Output(e)),
            Err(_) => Err(VoiceError::Output("le périphérique ne répond pas".into())),
        }
    }
}

fn open_stream(queue: Arc<Mutex<Queue>>) -> Result<(cpal::Stream, u32, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("aucun périphérique de sortie")?;
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "sortie par défaut".into());
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
    ) -> Result<cpal::Stream, String> {
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    let mut q = queue.lock().unwrap();
                    for frame in data.chunks_mut(channels) {
                        let v = q.samples.pop_front().unwrap_or(0.0);
                        for out in frame {
                            *out = T::from_sample(v);
                        }
                    }
                },
                |e| tracing::warn!("sortie audio : {e}"),
                None,
            )
            .map_err(|e| e.to_string())
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, channels, queue),
        SampleFormat::I16 => build::<i16>(&device, config, channels, queue),
        SampleFormat::I32 => build::<i32>(&device, config, channels, queue),
        SampleFormat::U16 => build::<u16>(&device, config, channels, queue),
        other => Err(format!("format d'échantillon non géré : {other}")),
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
        // Fondu de 15 ms pour éviter un clic.
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
        s.play(&Audio {
            samples: vec![0.0; 16_000],
            sample_rate: 16_000,
        });
        s.stop();
        assert!(!s.is_playing());
    }
}
