//! Owns a live mirroring session on a background thread: it pushes/starts the
//! scrcpy-server, connects, decodes, and publishes the latest frame for the GUI.
//!
//! Shutdown is designed to never block the UI thread: we keep clones of the
//! sockets so a stop can `shutdown()` them immediately (unblocking any in-flight
//! read), and the thread join is detached so reconnects feel instant.

use crate::adb::{self, ServerOptions};
use crate::decoder::{Frame, H264Decoder};
use crate::recorder::Recorder;
use crate::server;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub serial: String,
    pub display_id: u32,
    pub max_size: u32,
    pub video_bit_rate: u32,
    pub max_fps: u32,
    pub audio: bool,
    pub audio_bit_rate: u32,
}

#[derive(Clone, Debug)]
pub enum Status {
    Connecting,
    Streaming { width: u32, height: u32, fps: f32 },
    Error(String),
    Stopped,
}

/// Shared, latest-only frame slot. The decoder overwrites; the GUI takes.
#[derive(Default)]
pub struct FrameSlot {
    pub frame: Option<Frame>,
    pub generation: u64,
}

/// UI -> stream-thread recording commands.
enum RecordCmd {
    Start(PathBuf),
    Stop,
}

pub struct StreamHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Clones of the live sockets, used to interrupt blocking reads on stop.
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    record_tx: Sender<RecordCmd>,
    recording: Arc<AtomicBool>,
    pub slot: Arc<Mutex<FrameSlot>>,
    pub status: Arc<Mutex<Status>>,
    pub config: StreamConfig,
}

impl StreamHandle {
    pub fn start(config: StreamConfig, repaint: egui::Context) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(FrameSlot::default()));
        let status = Arc::new(Mutex::new(Status::Connecting));
        let sockets: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(false));
        let (record_tx, record_rx) = unbounded();

        let join = {
            let stop = stop.clone();
            let slot = slot.clone();
            let status = status.clone();
            let sockets = sockets.clone();
            let recording = recording.clone();
            let cfg = config.clone();
            std::thread::Builder::new()
                .name("scrcpy-stream".into())
                .spawn(move || {
                    let ctx = Ctx { stop: &stop, slot: &slot, status: &status, repaint: &repaint, sockets: &sockets, recording: &recording, record_rx: &record_rx };
                    if let Err(e) = run(&cfg, &ctx) {
                        if !stop.load(Ordering::Relaxed) {
                            *status.lock().unwrap() = Status::Error(format!("{e:#}"));
                            repaint.request_repaint();
                        }
                    }
                    recording.store(false, Ordering::Relaxed);
                })
                .expect("spawn stream thread")
        };

        Self {
            stop,
            join: Some(join),
            sockets,
            record_tx,
            recording,
            slot,
            status,
            config,
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    pub fn start_recording(&self, path: PathBuf) {
        let _ = self.record_tx.send(RecordCmd::Start(path));
    }

    pub fn stop_recording(&self) {
        let _ = self.record_tx.send(RecordCmd::Stop);
    }

    /// Set the stop flag and shut the sockets down so any blocking read returns
    /// at once. Cheap and non-blocking — safe to call from the UI thread.
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(socks) = self.sockets.lock() {
            for s in socks.iter() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.signal_stop();
        // Detach the join: device-side teardown (adb kill/unforward) runs on the
        // stream thread, so we don't want to stall the UI waiting for it.
        if let Some(j) = self.join.take() {
            std::thread::spawn(move || {
                let _ = j.join();
            });
        }
    }
}

/// Bundle of shared state handed to the stream thread.
struct Ctx<'a> {
    stop: &'a Arc<AtomicBool>,
    slot: &'a Arc<Mutex<FrameSlot>>,
    status: &'a Arc<Mutex<Status>>,
    repaint: &'a egui::Context,
    sockets: &'a Arc<Mutex<Vec<TcpStream>>>,
    recording: &'a Arc<AtomicBool>,
    record_rx: &'a Receiver<RecordCmd>,
}

/// Forward the device-side server's stdout/stderr to our log for diagnosis.
fn drain_child_logs(child: &mut std::process::Child) {
    use std::io::{BufRead, BufReader};
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                eprintln!("[server] {line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[server] {line}");
            }
        });
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(27183)
}

fn run(cfg: &StreamConfig, ctx: &Ctx) -> Result<()> {
    adb::push_server(&cfg.serial)?;
    let port = free_port();
    adb::forward(&cfg.serial, port)?;

    let opts = ServerOptions {
        display_id: cfg.display_id,
        max_size: cfg.max_size,
        video_bit_rate: cfg.video_bit_rate,
        max_fps: cfg.max_fps,
        audio: cfg.audio,
        audio_bit_rate: cfg.audio_bit_rate,
    };
    let mut child = adb::start_server(&cfg.serial, &opts)?;
    drain_child_logs(&mut child);

    // Always tear the device side down on the way out.
    let result = stream_loop(cfg, ctx, port);

    let _ = child.kill();
    adb::remove_forward(&cfg.serial, port);
    *ctx.status.lock().unwrap() = Status::Stopped;
    result
}

fn stream_loop(cfg: &StreamConfig, ctx: &Ctx, port: u16) -> Result<()> {
    // Connect video (+audio) together: the server only sends the device meta
    // once every requested socket has been accepted.
    let conn = server::connect(port, cfg.audio, Duration::from_secs(12))?;
    let server::Connection { mut video, video_meta, audio } = conn;
    eprintln!(
        "[stream] connected: device={:?} video={}",
        video_meta.device_name,
        server::codec_name(video_meta.codec_id)
    );

    // Register socket clones so a stop can interrupt blocking reads instantly.
    {
        let mut socks = ctx.sockets.lock().unwrap();
        if let Ok(c) = video.try_clone() {
            socks.push(c);
        }
        if let Some((s, _)) = &audio {
            if let Ok(c) = s.try_clone() {
                socks.push(c);
            }
        }
    }

    // Optional audio runs on its own socket/thread.
    let audio_handle = audio.map(|(sock, meta)| crate::audio::spawn(sock, meta, ctx.stop.clone()));

    // The decoder is built once we learn the resolution (a session-meta event).
    let mut decoder: Option<H264Decoder> = None;
    let mut sess_w = 0u32;
    let mut sess_h = 0u32;
    let mut frame_w = 0u32;
    let mut frame_h = 0u32;
    let mut frames_since = 0u32;
    let mut last_tick = Instant::now();

    // Recording state.
    let mut recorder: Option<Recorder> = None;
    let mut last_config: Vec<u8> = Vec::new(); // most recent SPS/PPS
    let mut pending_record: Option<PathBuf> = None; // start once we have config
    let mut waiting_key = false; // don't write until the first keyframe

    while !ctx.stop.load(Ordering::Relaxed) {
        // Apply any pending record commands first.
        while let Ok(cmd) = ctx.record_rx.try_recv() {
            match cmd {
                RecordCmd::Start(path) => {
                    if let Some(r) = recorder.take() {
                        let _ = r.finalize();
                    }
                    if !last_config.is_empty() && sess_w > 0 {
                        match Recorder::new(&path, sess_w, sess_h, cfg.max_fps, &last_config) {
                            Ok(r) => {
                                recorder = Some(r);
                                waiting_key = true;
                                ctx.recording.store(true, Ordering::Relaxed);
                                eprintln!("[record] started -> {}", path.display());
                            }
                            Err(e) => eprintln!("[record] failed to start: {e:#}"),
                        }
                    } else {
                        // No config yet: arm it, start when the next config arrives.
                        pending_record = Some(path);
                    }
                }
                RecordCmd::Stop => {
                    pending_record = None;
                    if let Some(r) = recorder.take() {
                        match r.finalize() {
                            Ok(()) => eprintln!("[record] saved"),
                            Err(e) => eprintln!("[record] finalize error: {e:#}"),
                        }
                    }
                    ctx.recording.store(false, Ordering::Relaxed);
                }
            }
        }

        let event = match server::read_event(&mut video) {
            Ok(e) => e,
            Err(e) => {
                if ctx.stop.load(Ordering::Relaxed) {
                    break;
                }
                return Err(e);
            }
        };

        match event {
            server::StreamEvent::Resolution { width, height } => {
                if decoder.is_none() || width != sess_w || height != sess_h {
                    sess_w = width;
                    sess_h = height;
                    decoder = Some(H264Decoder::new(width.max(1), height.max(1))?);
                    *ctx.status.lock().unwrap() = Status::Streaming { width, height, fps: 0.0 };
                    ctx.repaint.request_repaint();
                }
            }
            server::StreamEvent::Packet(pkt) => {
                if pkt.is_config {
                    last_config = pkt.data.clone();
                    // Honor a record request that came in before we had config.
                    if let Some(path) = pending_record.take() {
                        if sess_w > 0 {
                            match Recorder::new(&path, sess_w, sess_h, cfg.max_fps, &last_config) {
                                Ok(r) => {
                                    recorder = Some(r);
                                    waiting_key = true;
                                    ctx.recording.store(true, Ordering::Relaxed);
                                    eprintln!("[record] started -> {}", path.display());
                                }
                                Err(e) => eprintln!("[record] failed to start: {e:#}"),
                            }
                        } else {
                            pending_record = Some(path);
                        }
                    }
                }

                // Feed the recorder (config packets are folded into the first
                // keyframe by the recorder, so we only forward real frames).
                if let Some(rec) = recorder.as_mut() {
                    if !pkt.is_config {
                        if waiting_key && !pkt.is_key {
                            // skip leading non-keyframes for a clean start
                        } else {
                            waiting_key = false;
                            if let Err(e) = rec.write(&pkt.data, pkt.pts, pkt.is_key) {
                                eprintln!("[record] write error: {e:#}");
                                if let Some(r) = recorder.take() {
                                    let _ = r.finalize();
                                }
                                ctx.recording.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                }

                if decoder.is_none() {
                    let w = if sess_w > 0 { sess_w } else { 1920 };
                    let h = if sess_h > 0 { sess_h } else { 1088 };
                    sess_w = w;
                    sess_h = h;
                    decoder = Some(H264Decoder::new(w, h)?);
                }
                let dec = decoder.as_mut().unwrap();
                for frame in dec.decode(&pkt.data)? {
                    frame_w = frame.width;
                    frame_h = frame.height;
                    {
                        let mut s = ctx.slot.lock().unwrap();
                        s.frame = Some(frame);
                        s.generation = s.generation.wrapping_add(1);
                    }
                    frames_since += 1;
                    ctx.repaint.request_repaint();
                }

                if last_tick.elapsed() >= Duration::from_millis(500) {
                    let fps = frames_since as f32 / last_tick.elapsed().as_secs_f32();
                    frames_since = 0;
                    last_tick = Instant::now();
                    *ctx.status.lock().unwrap() = Status::Streaming {
                        width: frame_w.max(sess_w),
                        height: frame_h.max(sess_h),
                        fps,
                    };
                }
            }
        }
    }

    if let Some(r) = recorder.take() {
        let _ = r.finalize();
    }
    ctx.recording.store(false, Ordering::Relaxed);
    if let Some(h) = audio_handle {
        h.stop();
    }
    Ok(())
}
