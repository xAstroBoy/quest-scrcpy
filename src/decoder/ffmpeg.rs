//! H.264 decode by piping an Annex-B elementary stream into the `ffmpeg` CLI
//! and reading back raw RGBA frames. One long-lived ffmpeg process per decoder;
//! a reader thread collects whole frames so `decode()` never blocks on the pipe.
//!
//! We pin the output to the size scrcpy/the agent declared (`-s WxH`), so each
//! frame is exactly `W*H*4` bytes — the framing rawvideo otherwise lacks.

use anyhow::{Context, Result, anyhow};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use super::Frame;

pub struct FfmpegDecoder {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Vec<u8>>,
    width: u32,
    height: u32,
}

impl FfmpegDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);

        let mut cmd = crate::ffmpeg::command().ok_or_else(|| anyhow!("ffmpeg not available"))?;
        // Raw RGBA out at a fixed size. A modest probesize bounds startup
        // latency without starving the decoder of SPS/PPS. We deliberately do
        // NOT set `-flags low_delay`/`-fflags nobuffer`: over a pipe they make
        // the decoder emit nothing here, and the stream has no B-frames so the
        // default already returns each frame as soon as it's decoded.
        cmd.args(["-probesize", "500000"])
            .args(["-f", "h264", "-i", "pipe:0"])
            .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{width}x{height}")])
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("spawn ffmpeg (decode)")?;
        let stdin = Some(child.stdin.take().context("ffmpeg decode stdin")?);
        let mut stdout = child.stdout.take().context("ffmpeg decode stdout")?;
        crate::ffmpeg::drain_stderr(&mut child, "decode");

        // Reader thread: pull complete RGBA frames out of stdout and queue them.
        let frame_bytes = (width as usize) * (height as usize) * 4;
        let (tx, rx) = channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("ffmpeg-decode-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; frame_bytes];
                loop {
                    match stdout.read_exact(&mut buf) {
                        Ok(()) => {
                            if tx.send(buf.clone()).is_err() {
                                break; // decoder dropped
                            }
                        }
                        Err(_) => break, // EOF / process gone
                    }
                }
            })
            .context("spawn ffmpeg decode reader")?;

        Ok(Self { child, stdin, frames: rx, width, height })
    }

    /// Feed one access unit; return whatever frames ffmpeg has produced so far.
    /// Frames lag the input by ~one access unit, which is fine for live mirror.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<Frame>> {
        if let Some(stdin) = self.stdin.as_mut() {
            stdin
                .write_all(au)
                .and_then(|_| stdin.flush())
                .map_err(|e| anyhow!("ffmpeg decode pipe closed: {e}"))?;
        }

        let mut out = Vec::new();
        loop {
            match self.frames.try_recv() {
                Ok(rgba) => out.push(Frame { width: self.width, height: self.height, rgba }),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // ffmpeg exited — hand back any frames it produced first;
                    // a later (empty) call surfaces the error.
                    if out.is_empty() {
                        return Err(anyhow!("ffmpeg decode process exited"));
                    }
                    break;
                }
            }
        }
        Ok(out)
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
impl FfmpegDecoder {
    /// Close the input pipe (EOF) so ffmpeg flushes a finite clip — for tests
    /// fed a fixed-size stream rather than a live, never-ending one.
    fn close_input(&mut self) {
        self.stdin = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_h264_to_rgba() {
        if !crate::ffmpeg::available() {
            eprintln!("ffmpeg not on PATH; skipping decode round-trip");
            return;
        }
        // Build a tiny Annex-B H.264 clip with ffmpeg itself.
        let h264 = std::env::temp_dir().join("qs_ff_dec_test.h264");
        let _ = std::fs::remove_file(&h264);
        let status = crate::ffmpeg::command()
            .unwrap()
            .args(["-f", "lavfi", "-i", "testsrc=size=320x240:rate=10", "-t", "1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-f", "h264"])
            .arg("-y")
            .arg(&h264)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg failed to make test clip");

        let data = std::fs::read(&h264).expect("read h264");
        let mut dec = FfmpegDecoder::new(320, 240).expect("decoder");
        let mut frames = 0usize;
        for chunk in data.chunks(4096) {
            frames += dec.decode(chunk).expect("decode").len();
        }
        // Finite clip: signal EOF so ffmpeg flushes, then collect the frames.
        dec.close_input();
        std::thread::sleep(std::time::Duration::from_millis(400));
        frames += dec.decode(&[]).expect("drain").len();

        assert!(frames > 0, "expected at least one decoded frame, got {frames}");
        let _ = std::fs::remove_file(&h264);
    }
}
