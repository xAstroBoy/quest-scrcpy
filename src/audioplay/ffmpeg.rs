//! AAC -> PCM -> speakers via the `ffmpeg` CLI (cross-platform).
//!
//! scrcpy/the agent stream raw AAC-LC: a CONFIG packet (AudioSpecificConfig)
//! then raw access units. ffmpeg's CLI wants ADTS, so we wrap each access unit
//! in a 7-byte ADTS header (built from the ASC) and pipe it in; ffmpeg decodes
//! and resamples to the output device's rate/channels, which we feed to cpal.

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};

/// ~150 ms ceiling on queued audio (stereo @ 48 kHz) to bound A/V latency.
const MAX_QUEUED_SAMPLES: usize = (48_000 * 2 * 150) / 1000;

pub struct FfmpegPlayer {
    shared: Arc<Mutex<VecDeque<f32>>>,
    _stream: cpal::Stream,
    out_rate: u32,
    out_channels: u32,
    /// Live ffmpeg AAC decoder, (re)spawned on each CONFIG packet.
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// ADTS header fields parsed from the AudioSpecificConfig.
    adts: Option<crate::ffmpeg::Adts>,
}

impl FfmpegPlayer {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device"))?;
        let supported = device
            .default_output_config()
            .map_err(|e| anyhow!("default output config: {e}"))?;
        let out_rate = supported.sample_rate();
        let out_channels = supported.channels() as u32;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();

        let shared: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cb_buf = shared.clone();
        let err_fn = |e| eprintln!("[audio] stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config.clone(),
                move |data: &mut [f32], _| fill(&cb_buf, data),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                config.clone(),
                move |data: &mut [i16], _| {
                    let mut tmp = vec![0f32; data.len()];
                    fill(&cb_buf, &mut tmp);
                    for (o, s) in data.iter_mut().zip(tmp) {
                        *o = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                config.clone(),
                move |data: &mut [u16], _| {
                    let mut tmp = vec![0f32; data.len()];
                    fill(&cb_buf, &mut tmp);
                    for (o, s) in data.iter_mut().zip(tmp) {
                        let v = (s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32;
                        *o = v as u16;
                    }
                },
                err_fn,
                None,
            ),
            other => bail!("unsupported output sample format: {other:?}"),
        }
        .map_err(|e| anyhow!("build output stream: {e}"))?;

        stream.play().map_err(|e| anyhow!("stream.play: {e}"))?;
        eprintln!("[audio] output {out_rate} Hz x{out_channels} ({sample_format:?}) via ffmpeg");

        Ok(Self {
            shared,
            _stream: stream,
            out_rate,
            out_channels,
            child: None,
            stdin: None,
            adts: None,
        })
    }

    pub fn feed(&mut self, data: &[u8], is_config: bool) -> Result<()> {
        if is_config {
            self.adts = Some(crate::ffmpeg::Adts::from_asc(data));
            self.spawn_decoder()?;
            return Ok(());
        }
        let Some(adts) = self.adts else {
            return Ok(()); // haven't seen the config yet
        };
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        let header = adts.header(data.len());
        if stdin.write_all(&header).and_then(|_| stdin.write_all(data)).is_err() {
            // ffmpeg died; tear down so the next CONFIG respawns it.
            self.stdin = None;
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
            }
        }
        Ok(())
    }

    fn spawn_decoder(&mut self) -> Result<()> {
        // Replace any previous decoder process.
        self.stdin = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }

        let mut cmd = crate::ffmpeg::command().ok_or_else(|| anyhow!("ffmpeg not available"))?;
        cmd.args(["-f", "aac", "-i", "pipe:0"])
            .args(["-f", "f32le"])
            .args(["-ar", &self.out_rate.to_string()])
            .args(["-ac", &self.out_channels.to_string()])
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| anyhow!("spawn ffmpeg (audio): {e}"))?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("ffmpeg audio stdout"))?;
        crate::ffmpeg::drain_stderr(&mut child, "audio");

        // Reader thread: f32le PCM (already at device rate/channels) -> queue.
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("ffmpeg-audio-read".into())
            .spawn(move || {
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            acc.extend_from_slice(&buf[..n]);
                            let usable = acc.len() - (acc.len() % 4);
                            if usable == 0 {
                                continue;
                            }
                            let mut q = shared.lock().unwrap();
                            if q.len() > MAX_QUEUED_SAMPLES {
                                q.clear();
                            }
                            for ch in acc[..usable].chunks_exact(4) {
                                q.push_back(f32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]));
                            }
                            drop(q);
                            acc.drain(..usable);
                        }
                    }
                }
            })
            .ok();

        self.child = Some(child);
        self.stdin = stdin;
        Ok(())
    }
}

impl Drop for FfmpegPlayer {
    fn drop(&mut self) {
        self.stdin = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn fill(shared: &Arc<Mutex<VecDeque<f32>>>, data: &mut [f32]) {
    let mut q = shared.lock().unwrap();
    for d in data.iter_mut() {
        *d = q.pop_front().unwrap_or(0.0);
    }
}
