//! Clip recording through the `ffmpeg` CLI (cross-platform).
//!
//! - [`FfmpegMux`] copies a raw H.264 elementary stream straight into MP4
//!   (`-c:v copy`, lossless) — the headless CLI path.
//! - [`FfmpegEncoder`] re-encodes processed BGRA frames with libx264, matching
//!   what's on screen — the GUI "Record" path.

use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// AAC captured alongside the video, written as an ADTS `.aac` sidecar and
/// muxed into the final MP4 at finalize().
struct AudioSidecar {
    path: PathBuf,
    file: std::fs::File,
    adts: crate::ffmpeg::Adts,
}

/// Re-encode processed BGRA frames (top-down) to MP4 via libx264, optionally
/// muxing in the captured AAC audio.
///
/// Video is encoded live to a temp MP4; audio access units are appended to a
/// temp ADTS file. On finalize we either rename the video to the target (no
/// audio) or run a quick `-c copy` mux of video+audio. A/V start together, so
/// they stay roughly in sync (video is CFR at `fps`).
pub struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    width: u32,
    height: u32,
    frame_bytes: usize,
    final_path: PathBuf,
    video_tmp: PathBuf,
    audio: Option<AudioSidecar>,
}

impl FfmpegEncoder {
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, bitrate: u32) -> Result<Self> {
        let width = width.max(2) & !1; // H.264 needs even dimensions
        let height = height.max(2) & !1;
        let _ = fps_hint; // frame timing comes from wall-clock, not a fixed rate
        let bitrate = if bitrate > 0 { bitrate } else { 12_000_000 };
        let video_tmp = path.with_extension("video.tmp.mp4");

        // Timestamp frames by wall-clock arrival, NOT a fixed input rate: the
        // flat stream's real frame-rate is variable and usually below the nominal
        // fps, so a fixed `-r` would compress the video in time (played back
        // sped-up) and drift out of sync with the real-time audio. Wall-clock
        // stamps + VFR output keep the recording at true speed.
        let mut cmd = crate::ffmpeg::command().ok_or_else(|| anyhow!("ffmpeg not available"))?;
        cmd.args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args(["-s", &format!("{width}x{height}")])
            .args(["-use_wallclock_as_timestamps", "1"])
            .args(["-i", "pipe:0"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"])
            .args(["-b:v", &bitrate.to_string()])
            .args(["-fps_mode", "vfr"])
            .arg("-y")
            .arg(&video_tmp)
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
            final_path: path.to_path_buf(),
            video_tmp,
            audio: None,
        })
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Begin capturing audio: open the ADTS sidecar using this AudioSpecificConfig.
    pub fn set_audio_config(&mut self, asc: &[u8]) {
        if self.audio.is_some() {
            return; // already capturing
        }
        let path = self.final_path.with_extension("audio.tmp.aac");
        match std::fs::File::create(&path) {
            Ok(file) => {
                self.audio = Some(AudioSidecar { path, file, adts: crate::ffmpeg::Adts::from_asc(asc) });
            }
            Err(e) => eprintln!("[flat] audio sidecar create failed (video only): {e}"),
        }
    }

    /// Append one raw AAC access unit (wrapped in ADTS) to the audio sidecar.
    pub fn write_audio(&mut self, au: &[u8]) {
        if let Some(a) = self.audio.as_mut() {
            let _ = a.file.write_all(&a.adts.header(au.len()));
            let _ = a.file.write_all(au);
        }
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
            let _ = std::fs::remove_file(&self.video_tmp);
            return Err(anyhow!("ffmpeg encode exited with {status}"));
        }

        // No audio → the temp video IS the recording.
        let audio = match self.audio.take() {
            Some(a) => a,
            None => {
                std::fs::rename(&self.video_tmp, &self.final_path)
                    .context("move recording into place")?;
                return Ok(());
            }
        };
        drop(audio.file); // flush sidecar

        // Mux video + audio (no re-encode). If it fails, keep the video alone.
        let muxed = crate::ffmpeg::command()
            .ok_or_else(|| anyhow!("ffmpeg not available"))
            .and_then(|mut c| {
                c.arg("-i")
                    .arg(&self.video_tmp)
                    .arg("-i")
                    .arg(&audio.path)
                    .args(["-c", "copy", "-shortest"])
                    .arg("-y")
                    .arg(&self.final_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
                let mut ch = c.spawn().context("spawn ffmpeg (mux a/v)")?;
                crate::ffmpeg::drain_stderr(&mut ch, "mux-av");
                Ok(ch.wait().context("wait ffmpeg mux a/v")?.success())
            });
        let _ = std::fs::remove_file(&audio.path);
        match muxed {
            Ok(true) => {
                let _ = std::fs::remove_file(&self.video_tmp);
                Ok(())
            }
            other => {
                if let Err(e) = &other {
                    eprintln!("[flat] a/v mux failed, keeping video only: {e:#}");
                } else {
                    eprintln!("[flat] a/v mux failed, keeping video only");
                }
                std::fs::rename(&self.video_tmp, &self.final_path)
                    .context("move video-only recording into place")?;
                Ok(())
            }
        }
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

    /// Split an ADTS .aac into raw access units + reconstruct the ASC, so we can
    /// drive the encoder's audio path the way the agent's AAC stream does.
    fn split_adts(data: &[u8]) -> (Vec<Vec<u8>>, [u8; 2]) {
        let mut aus = Vec::new();
        let mut asc = [0u8, 0u8];
        let mut i = 0;
        while i + 7 <= data.len() {
            if data[i] == 0xFF && (data[i + 1] & 0xF0) == 0xF0 {
                let profile = (data[i + 2] >> 6) & 0x3;
                let freq = (data[i + 2] >> 2) & 0xF;
                let chan = ((data[i + 2] & 0x1) << 2) | ((data[i + 3] >> 6) & 0x3);
                let frame_len = (((data[i + 3] & 0x3) as usize) << 11)
                    | ((data[i + 4] as usize) << 3)
                    | ((data[i + 5] as usize) >> 5);
                if frame_len < 7 || i + frame_len > data.len() {
                    break;
                }
                let obj = profile + 1;
                asc = [(obj << 3) | (freq >> 1), ((freq & 1) << 7) | (chan << 3)];
                aus.push(data[i + 7..i + frame_len].to_vec());
                i += frame_len;
            } else {
                i += 1;
            }
        }
        (aus, asc)
    }

    #[test]
    fn ffmpeg_muxes_audio_into_recording() {
        if !crate::ffmpeg::available() {
            eprintln!("ffmpeg not on PATH; skipping audio-mux test");
            return;
        }
        // Real AAC (ADTS) to feed as the agent's audio would arrive.
        let aac = std::env::temp_dir().join("qs_ff_aud.aac");
        let _ = std::fs::remove_file(&aac);
        let ok = crate::ffmpeg::command()
            .unwrap()
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=1"])
            .args(["-c:a", "aac", "-f", "adts"])
            .arg("-y")
            .arg(&aac)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "failed to make test aac");
        let (aus, asc) = split_adts(&std::fs::read(&aac).expect("read aac"));
        assert!(!aus.is_empty(), "no AAC access units parsed");

        let path = std::env::temp_dir().join("qs_ff_av.mp4");
        let _ = std::fs::remove_file(&path);
        let (w, h) = (320u32, 240u32);
        let mut enc = FfmpegEncoder::new(&path, w, h, 30, 2_000_000).expect("encoder");
        enc.set_audio_config(&asc);
        let buf = vec![80u8; (w * h * 4) as usize];
        for (f, au) in aus.iter().enumerate() {
            // ~roughly one video frame per audio AU for the test.
            enc.write_bgra(&buf, (f as u64) * 33_333).expect("write frame");
            enc.write_audio(au);
        }
        enc.finalize().expect("finalize");

        // ffprobe: the output must carry an audio stream alongside video.
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .expect("run ffprobe");
        let kinds = String::from_utf8_lossy(&probe.stdout);
        assert!(kinds.contains("video"), "no video stream: {kinds:?}");
        assert!(kinds.contains("audio"), "no audio stream in recording: {kinds:?}");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&aac);
    }
}
