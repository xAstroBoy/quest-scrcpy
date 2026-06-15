//! AAC playback with two interchangeable backends behind one API:
//!
//! - [`ffmpeg`] — wraps access units in ADTS and pipes them through the
//!   `ffmpeg` CLI (cross-platform; chosen when ffmpeg is on PATH).
//! - [`mf`] — the built-in Media Foundation AAC decoder (Windows fallback).
//!
//! Both decode AAC-LC and play through cpal; callers just use [`AacPlayer`].

mod ffmpeg;
#[cfg(windows)]
mod mf;

use anyhow::Result;

pub enum AacPlayer {
    Ffmpeg(ffmpeg::FfmpegPlayer),
    #[cfg(windows)]
    Mf(mf::MfPlayer),
}

impl AacPlayer {
    pub fn new() -> Result<Self> {
        if crate::ffmpeg::available() {
            return Ok(Self::Ffmpeg(ffmpeg::FfmpegPlayer::new()?));
        }
        #[cfg(windows)]
        {
            return Ok(Self::Mf(mf::MfPlayer::new()?));
        }
        #[cfg(not(windows))]
        anyhow::bail!("no audio backend — install ffmpeg and make sure it is on your PATH")
    }

    /// Feed an access unit, or the AudioSpecificConfig when `is_config`.
    pub fn feed(&mut self, data: &[u8], is_config: bool) -> Result<()> {
        match self {
            Self::Ffmpeg(p) => p.feed(data, is_config),
            #[cfg(windows)]
            Self::Mf(p) => p.feed(data, is_config),
        }
    }
}
