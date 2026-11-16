//! Audio device listing and lookup.
//!
//! A device is saved by its cpal id (stable across launches, e.g. the WASAPI
//! endpoint id on Windows); the name is only for display. `None` means the
//! system default device.

use cpal::traits::{DeviceTrait, HostTrait};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

fn describe(d: &cpal::Device) -> Option<AudioDevice> {
    Some(AudioDevice {
        id: d.id().ok()?.to_string(),
        name: d
            .description()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "unnamed device".into()),
    })
}

/// Devices available in that direction, in the host's order.
pub fn list(direction: Direction) -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let devices = match direction {
        Direction::Input => host.input_devices().map(|d| d.collect::<Vec<_>>()),
        Direction::Output => host.output_devices().map(|d| d.collect::<Vec<_>>()),
    };
    devices
        .map(|v| v.iter().filter_map(describe).collect())
        .unwrap_or_default()
}

/// The device with that id, or the default device for `None`.
pub(crate) fn find(direction: Direction, id: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match id {
        None => match direction {
            Direction::Input => host.default_input_device().ok_or("no microphone".into()),
            Direction::Output => host
                .default_output_device()
                .ok_or("no output device".into()),
        },
        Some(id) => {
            let parsed: cpal::DeviceId =
                id.parse().map_err(|e| format!("device id {id:?}: {e}"))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| format!("device not found: {id}"))
        }
    }
}
