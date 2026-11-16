//! The media stack of meow apps: everything between "bytes of a stream" and "picture on the
//! TV, sound on HDMI" for H.264 + AAC/PCM on the RK3228A dongle. See ../README.md.
//!
//! Licence: MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE), like the SDK. Its AAC decoder is
//! the symphonia crate (MPL-2.0), which an app that links this library then also contains.
//!
//! ```text
//!  mp4 (fMP4 demux) ─┐                 ┌─ h264 ─ v4l2 ─ decoder (rkvdec) ─┐
//!  es (Annex B, ADTS)┼─► player::play ─┤                                  ├─► video::Display
//!  your own demuxer ─┘   (A/V sync)    └─ audio (AAC, sound card)         │   (picture plane +
//!                                                       canvas (app UI) ──┘    UI overlay)
//! ```

pub mod audio;
pub mod canvas;
pub mod decoder;
pub mod es;
pub mod h264;
pub mod hls;
pub mod image;
pub mod mp4;
pub mod player;
pub mod png;
pub mod ts;
pub mod v4l2;
pub mod video;
