//! # tapestry-core
//!
//! Everything Tapestry does that isn't drawing pixels: talking to Tapo
//! cameras over ONVIF (pan/tilt, motion events), pulling video through
//! FFmpeg, recording, and managing the recordings folder.
//!
//! Deliberately dependency-free (std only), so it builds offline and is easy
//! to audit.

pub mod audio;
pub mod clips;
pub mod color;
pub mod config;
pub mod crypto;
pub mod engine;
pub mod ffmpeg;
pub mod http;
pub mod link;
pub mod onvif;
pub mod recorder;
pub mod stream;
pub mod time;
pub mod xml;

pub use audio::AudioStream;
pub use color::ColorInfo;
pub use config::{Camera, Config, Quality, RecordMode};
pub use engine::{CameraState, Engine, EngineOptions, LinkState, LogEntry, LogLevel};
pub use link::PtzCommand;
pub use stream::{Frame, StreamSpec, StreamStatus, VideoStream};
