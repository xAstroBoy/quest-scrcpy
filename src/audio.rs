//! Audio sidecar: plays back the scrcpy audio socket.
//!
//! The socket is connected up front by [`server::connect`] (the handshake must
//! connect video + audio together), so we just receive an already-open stream
//! plus its codec meta and pump packets into the AAC player.

use crate::server::{self, AudioMeta, StreamEvent};
use anyhow::{Result, bail};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AudioHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Start playback on a background thread. `stop` is shared with the video stream
/// so both end together.
pub fn spawn(sock: TcpStream, meta: AudioMeta, stop: Arc<AtomicBool>) -> AudioHandle {
    let local_stop = stop.clone();
    let join = std::thread::Builder::new()
        .name("scrcpy-audio".into())
        .spawn(move || {
            if let Err(e) = run(sock, meta, &local_stop) {
                eprintln!("[audio] {e:#}");
            }
        })
        .ok();
    AudioHandle { stop, join }
}

fn run(mut sock: TcpStream, meta: AudioMeta, stop: &Arc<AtomicBool>) -> Result<()> {
    match meta.codec_id {
        server::AUDIO_DISABLED => {
            eprintln!("[audio] device declined audio capture; video only");
            return Ok(());
        }
        server::AUDIO_CONFIG_ERROR => bail!("device reported an audio configuration error"),
        server::CODEC_ID_AAC => {}
        other => bail!("unexpected audio codec 0x{other:08x} (expected AAC)"),
    }
    eprintln!("[audio] codec={}", server::codec_name(meta.codec_id));

    let mut player = crate::audioplay::AacPlayer::new()?;
    while !stop.load(Ordering::Relaxed) {
        match server::read_event(&mut sock) {
            Ok(StreamEvent::Packet(pkt)) => {
                if let Err(e) = player.feed(&pkt.data, pkt.is_config) {
                    eprintln!("[audio] feed error: {e:#}");
                    break;
                }
            }
            // Resolution events are video-only; never expected here.
            Ok(StreamEvent::Resolution { .. }) => {}
            Err(_) => break,
        }
    }
    Ok(())
}
