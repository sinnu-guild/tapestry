//! # tapestry-core
//!
//! Everything Tapestry does that isn't drawing pixels: talking to Tapo
//! cameras over ONVIF (pan/tilt, motion events), pulling video through
//! FFmpeg, recording, and managing the recordings folder.
//!
//! Deliberately dependency-free (std only), so it builds offline and is easy
//! to audit.

pub mod http;
pub mod xml;

