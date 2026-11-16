//! iRacing telemetry: live reading (Windows shared memory) and .ibt file
//! replay, lap splitting, corner detection, resampling.

pub mod corners;
pub mod hub;
pub mod ibt;
pub mod irsdk;
pub mod laps;
pub mod live;
pub mod sample;
pub mod session;
pub mod source;
pub mod synthetic;
pub mod trace;

pub use corners::{Corner, CornerDetectionParams, Direction, detect_corners};
pub use hub::{
    ConnectionState, LiveSnapshot, SnapshotHandle, TelemetryEvent, TelemetryHub, TelemetryHubConfig,
};
pub use ibt::IbtFile;
pub use laps::{Lap, LapEvent, LapFlags, LapRecorder, LapRecorderConfig};
pub use live::LiveSource;
pub use sample::{Sample, TrackSurface};
pub use session::SessionInfo;
pub use source::{ReplaySource, SourceEvent, TelemetrySource};
