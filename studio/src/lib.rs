//! Post-processing for recordings made without a cursor.
//!
//! The recorder logs mouse events next to the raw video; this crate turns the pair into the
//! finished clip: a virtual camera that zooms to clicks, a re-drawn smooth cursor with click
//! ripples, and a styled frame (gradient background, padding, rounded corners, shadow).

mod export;
mod render;
mod track;
mod types;

pub use export::{command, export, parse_info, preview, probe, VideoInfo};
pub use render::Compositor;
pub use track::{build as build_track, Frame};
pub use types::{parse_hex, InputEvent, Kind, Settings};
