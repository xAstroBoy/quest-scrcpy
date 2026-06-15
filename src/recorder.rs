//! Clip recording with two interchangeable backends behind one API:
//!
//! - [`ffmpeg`] — the `ffmpeg` CLI (cross-platform; chosen when ffmpeg is on PATH).
//! - [`mf`] — the Media Foundation sink writer (Windows fallback).
//!
//! [`Recorder`] muxes the H.264 elementary stream straight to MP4 (headless CLI,
//! lossless). [`ClipEncoder`] re-encodes processed BGRA frames — what's on screen.

mod ffmpeg;
#[cfg(windows)]
mod mf;

use anyhow::Result;
use std::path::Path;

pub enum Recorder {
    Ffmpeg(ffmpeg::FfmpegMux),
    #[cfg(windows)]
    Mf(mf::Recorder),
}

impl Recorder {
    /// `config` is the Annex-B SPS/PPS from the stream's config packet (MF only).
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, config: &[u8]) -> Result<Self> {
        if crate::ffmpeg::available() {
            return Ok(Self::Ffmpeg(ffmpeg::FfmpegMux::new(path, fps_hint)?));
        }
        #[cfg(windows)]
        {
            return Ok(Self::Mf(mf::Recorder::new(path, width, height, fps_hint, config)?));
        }
        #[cfg(not(windows))]
        {
            let _ = (width, height, config);
            anyhow::bail!("no recorder backend — install ffmpeg and make sure it is on your PATH")
        }
    }

    pub fn write(&mut self, au: &[u8], pts_us: u64, key: bool) -> Result<()> {
        match self {
            Self::Ffmpeg(r) => r.write(au, pts_us, key),
            #[cfg(windows)]
            Self::Mf(r) => r.write(au, pts_us, key),
        }
    }

    pub fn finalize(self) -> Result<()> {
        match self {
            Self::Ffmpeg(r) => r.finalize(),
            #[cfg(windows)]
            Self::Mf(r) => r.finalize(),
        }
    }
}

pub enum ClipEncoder {
    Ffmpeg(ffmpeg::FfmpegEncoder),
    #[cfg(windows)]
    Mf(mf::ClipEncoder),
}

impl ClipEncoder {
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, bitrate: u32) -> Result<Self> {
        if crate::ffmpeg::available() {
            return Ok(Self::Ffmpeg(ffmpeg::FfmpegEncoder::new(
                path, width, height, fps_hint, bitrate,
            )?));
        }
        #[cfg(windows)]
        {
            return Ok(Self::Mf(mf::ClipEncoder::new(path, width, height, fps_hint, bitrate)?));
        }
        #[cfg(not(windows))]
        anyhow::bail!("no recorder backend — install ffmpeg and make sure it is on your PATH")
    }

    pub fn dims(&self) -> (u32, u32) {
        match self {
            Self::Ffmpeg(e) => e.dims(),
            #[cfg(windows)]
            Self::Mf(e) => e.dims(),
        }
    }

    /// Start capturing audio into the recording (AudioSpecificConfig). Only the
    /// ffmpeg backend muxes audio; MF is video-only.
    pub fn set_audio_config(&mut self, asc: &[u8]) {
        match self {
            Self::Ffmpeg(e) => e.set_audio_config(asc),
            #[cfg(windows)]
            Self::Mf(e) => e.set_audio_config(asc),
        }
    }

    /// Append one raw AAC access unit to the recording's audio track.
    pub fn write_audio(&mut self, au: &[u8]) {
        match self {
            Self::Ffmpeg(e) => e.write_audio(au),
            #[cfg(windows)]
            Self::Mf(e) => e.write_audio(au),
        }
    }

    pub fn write_bgra(&mut self, bgra: &[u8], pts_us: u64) -> Result<()> {
        match self {
            Self::Ffmpeg(e) => e.write_bgra(bgra, pts_us),
            #[cfg(windows)]
            Self::Mf(e) => e.write_bgra(bgra, pts_us),
        }
    }

    pub fn finalize(self) -> Result<()> {
        match self {
            Self::Ffmpeg(e) => e.finalize(),
            #[cfg(windows)]
            Self::Mf(e) => e.finalize(),
        }
    }
}
