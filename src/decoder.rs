//! H.264 decode with two interchangeable backends behind one API:
//!
//! - [`ffmpeg`] — pipes Annex-B into the `ffmpeg` CLI (cross-platform; chosen
//!   whenever ffmpeg is on PATH).
//! - [`mf`] — the built-in Windows Media Foundation decoder (Windows fallback
//!   when ffmpeg isn't present).
//!
//! Callers use [`H264Decoder`] / [`Frame`] and don't care which is live.

mod ffmpeg;
#[cfg(windows)]
mod mf;

use anyhow::Result;

/// A decoded frame, tightly packed RGBA8 (`width * height * 4`).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub enum H264Decoder {
    Ffmpeg(ffmpeg::FfmpegDecoder),
    #[cfg(windows)]
    Mf(mf::MfDecoder),
}

impl H264Decoder {
    /// `expect_w`/`expect_h` is the declared display size; output is pinned to it.
    pub fn new(expect_w: u32, expect_h: u32) -> Result<Self> {
        if crate::ffmpeg::use_for_playback() {
            return Ok(Self::Ffmpeg(ffmpeg::FfmpegDecoder::new(expect_w, expect_h)?));
        }
        #[cfg(windows)]
        {
            return Ok(Self::Mf(mf::MfDecoder::new(expect_w, expect_h)?));
        }
        #[cfg(not(windows))]
        anyhow::bail!(
            "no H.264 decoder available — install ffmpeg and make sure it is on your PATH"
        )
    }

    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<Frame>> {
        match self {
            Self::Ffmpeg(d) => d.decode(au),
            #[cfg(windows)]
            Self::Mf(d) => d.decode(au),
        }
    }
}
