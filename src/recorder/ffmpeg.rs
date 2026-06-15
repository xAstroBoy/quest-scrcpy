//! Clip recording through the `ffmpeg` CLI (cross-platform).
//!
//! - [`FfmpegMux`] copies a raw H.264 elementary stream straight into MP4
//!   (`-c:v copy`, lossless) — the headless CLI path.
//! - [`FfmpegEncoder`] re-encodes processed BGRA frames with libx264, matching
//!   what's on screen — the GUI "Record" path.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};

/// Mux a raw H.264 elementary stream into MP4 with no re-encode.
pub struct FfmpegMux {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl FfmpegMux {
    pub fn new(path: &Path, fps_hint: u32) -> Result<Self> {
        let fps = if fps_hint > 0 { fps_hint } else { 60 };
        let mut cmd = crate::ffmpeg::command().ok_or_else(|| anyhow!("ffmpeg not available"))?;
        cmd.args(["-fflags", "+genpts"])
            .args(["-r", &fps.to_string()])
            .args(["-f", "h264", "-i", "pipe:0"])
            .args(["-c:v", "copy"])
            .arg("-y")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn ffmpeg (mux)")?;
        let stdin = child.stdin.take();
        crate::ffmpeg::drain_stderr(&mut child, "mux");
        Ok(Self { child, stdin })
    }

    pub fn write(&mut self, au: &[u8], _pts_us: u64, _key: bool) -> Result<()> {
        if let Some(si) = self.stdin.as_mut() {
            si.write_all(au).context("write to ffmpeg mux")?;
        }
        Ok(())
    }

    pub fn finalize(mut self) -> Result<()> {
        drop(self.stdin.take()); // EOF → ffmpeg writes the moov box and exits
        let status = self.child.wait().context("wait for ffmpeg mux")?;
        if !status.success() {
            return Err(anyhow!("ffmpeg mux exited with {status}"));
        }
        Ok(())
    }
}

/// Re-encode processed BGRA frames (top-down) to MP4 via libx264.
pub struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    width: u32,
    height: u32,
    frame_bytes: usize,
}

impl FfmpegEncoder {
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, bitrate: u32) -> Result<Self> {
        let width = width.max(2) & !1; // H.264 needs even dimensions
        let height = height.max(2) & !1;
        let fps = if fps_hint > 0 { fps_hint } else { 60 };
        let bitrate = if bitrate > 0 { bitrate } else { 12_000_000 };

        let mut cmd = crate::ffmpeg::command().ok_or_else(|| anyhow!("ffmpeg not available"))?;
        cmd.args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-s", &format!("{width}x{height}")])
            .args(["-r", &fps.to_string()])
            .args(["-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"])
            .args(["-b:v", &bitrate.to_string()])
            .arg("-y")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn ffmpeg (encode)")?;
        let stdin = child.stdin.take();
        crate::ffmpeg::drain_stderr(&mut child, "encode");
        Ok(Self {
            child,
            stdin,
            width,
            height,
            frame_bytes: (width as usize) * (height as usize) * 4,
        })
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// `bgra` must be at least `width*height*4` bytes (B,G,R,X), top-down.
    /// `pts_us` is ignored — we record at a constant `fps` (rawvideo has no PTS).
    pub fn write_bgra(&mut self, bgra: &[u8], _pts_us: u64) -> Result<()> {
        if bgra.len() < self.frame_bytes {
            return Err(anyhow!("frame buffer too small: {} < {}", bgra.len(), self.frame_bytes));
        }
        if let Some(si) = self.stdin.as_mut() {
            si.write_all(&bgra[..self.frame_bytes]).context("write frame to ffmpeg")?;
        }
        Ok(())
    }

    pub fn finalize(mut self) -> Result<()> {
        drop(self.stdin.take()); // EOF → flush encoder + write moov box
        let status = self.child.wait().context("wait for ffmpeg encode")?;
        if !status.success() {
            return Err(anyhow!("ffmpeg encode exited with {status}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_encodes_a_valid_mp4() {
        if !crate::ffmpeg::available() {
            eprintln!("ffmpeg not on PATH; skipping encode test");
            return;
        }
        let path = std::env::temp_dir().join("qs_ff_enc_test.mp4");
        let _ = std::fs::remove_file(&path);
        let (w, h) = (320u32, 240u32);
        let mut enc = FfmpegEncoder::new(&path, w, h, 30, 2_000_000).expect("create encoder");
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for f in 0..30u32 {
            for px in buf.chunks_mut(4) {
                px[0] = (f * 8) as u8; // B
                px[1] = 128; // G
                px[2] = 64; // R
                px[3] = 255;
            }
            enc.write_bgra(&buf, (f as u64) * 33_333).expect("write frame");
        }
        enc.finalize().expect("finalize");

        let bytes = std::fs::read(&path).expect("read mp4");
        assert!(bytes.len() > 1000, "mp4 too small: {}", bytes.len());
        assert_eq!(&bytes[4..8], b"ftyp", "not an mp4 container");
        let _ = std::fs::remove_file(&path);
    }
}
