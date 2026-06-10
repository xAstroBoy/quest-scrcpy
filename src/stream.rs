//! Owns a live mirroring session on a background thread: it pushes/starts the
//! scrcpy-server, connects, decodes, and publishes the latest frame for the GUI.
//!
//! Shutdown is designed to never block the UI thread: we keep clones of the
//! sockets so a stop can `shutdown()` them immediately (unblocking any in-flight
//! read), and the thread join is detached so reconnects feel instant.

use crate::adb::{self, ServerOptions};
use crate::decoder::{Frame, H264Decoder};
use crate::recorder::ClipEncoder;
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

/// The crop + lens-flatten + tilt the GUI is showing, so a recording can match
/// exactly what's on screen. `uv` is [min_x, min_y, max_x, max_y].
#[derive(Clone, Copy, Debug)]
pub struct ViewParams {
    pub uv: [f32; 4],
    pub lens_correct: bool,
    pub k1: f32,
    pub k2: f32,
    pub rotation_deg: f32,
}

impl Default for ViewParams {
    fn default() -> Self {
        Self { uv: [0.0, 0.0, 1.0, 1.0], lens_correct: false, k1: 0.0, k2: 0.0, rotation_deg: 0.0 }
    }
}

/// UI -> stream-thread recording commands. `Start` carries the view to capture.
enum RecordCmd {
    Start(PathBuf, ViewParams),
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

    pub fn start_recording(&self, path: PathBuf, view: ViewParams) {
        let _ = self.record_tx.send(RecordCmd::Start(path, view));
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

    // Recording state. The clip is encoded from the *processed* (cropped, lens-
    // flattened, rotated) view so it matches what's on screen. Its dimensions are
    // fixed at start, so we wait for the first decoded frame to size the encoder.
    let mut clip: Option<ClipEncoder> = None;
    let mut rec_params = ViewParams::default();
    let mut pending_record: Option<(PathBuf, ViewParams)> = None;

    while !ctx.stop.load(Ordering::Relaxed) {
        // Apply any pending record commands first.
        while let Ok(cmd) = ctx.record_rx.try_recv() {
            match cmd {
                RecordCmd::Start(path, view) => {
                    if let Some(c) = clip.take() {
                        let _ = c.finalize();
                    }
                    // Arm it; the encoder is created on the next decoded frame,
                    // once we know the source frame size to crop from.
                    pending_record = Some((path, view));
                    rec_params = view;
                }
                RecordCmd::Stop => {
                    pending_record = None;
                    if let Some(c) = clip.take() {
                        match c.finalize() {
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

                    // Start a pending recording now that we know the frame size.
                    if let Some((path, view)) = pending_record.take() {
                        let (ow, oh) = output_dims(&view, frame.width, frame.height);
                        match ClipEncoder::new(&path, ow, oh, cfg.max_fps, cfg.video_bit_rate) {
                            Ok(c) => {
                                rec_params = view;
                                clip = Some(c);
                                ctx.recording.store(true, Ordering::Relaxed);
                                eprintln!("[record] started -> {} ({ow}x{oh})", path.display());
                            }
                            Err(e) => eprintln!("[record] failed to start: {e:#}"),
                        }
                    }

                    // Encode the processed (cropped/flattened) view of this frame.
                    if let Some(enc) = clip.as_mut() {
                        let (ow, oh) = enc.dims();
                        let bgra = warp_to_bgra(&frame, &rec_params, ow, oh);
                        if let Err(e) = enc.write_bgra(&bgra, pkt.pts) {
                            eprintln!("[record] write error: {e:#}");
                            if let Some(c) = clip.take() {
                                let _ = c.finalize();
                            }
                            ctx.recording.store(false, Ordering::Relaxed);
                        }
                    }

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

    if let Some(c) = clip.take() {
        let _ = c.finalize();
    }
    ctx.recording.store(false, Ordering::Relaxed);
    if let Some(h) = audio_handle {
        h.stop();
    }
    Ok(())
}

/// Output (recording) dimensions for a crop, rounded to even (H.264 needs it).
pub fn output_dims(p: &ViewParams, frame_w: u32, frame_h: u32) -> (u32, u32) {
    let w = (((p.uv[2] - p.uv[0]) * frame_w as f32).round() as u32).max(2);
    let h = (((p.uv[3] - p.uv[1]) * frame_h as f32).round() as u32).max(2);
    (w & !1, h & !1)
}

/// Render the processed (cropped + lens-flattened + rotated) view of `frame`
/// into a top-down BGRA buffer of `out_w`×`out_h` — the inverse of the GPU mesh
/// in the GUI, so a recording matches what's on screen. Parallelised by rows.
pub fn warp_to_bgra(frame: &Frame, p: &ViewParams, out_w: u32, out_h: u32) -> Vec<u8> {
    let fw = frame.width as f32;
    let fh = frame.height as f32;
    let (k1, k2) = if p.lens_correct { (p.k1, p.k2) } else { (0.0, 0.0) };
    let (sin, cos) = p.rotation_deg.to_radians().sin_cos();
    let crop_min_x = p.uv[0] * fw;
    let crop_min_y = p.uv[1] * fh;
    let crop_w = (p.uv[2] - p.uv[0]) * fw;
    let crop_h = (p.uv[3] - p.uv[1]) * fh;

    let ow = out_w as usize;
    let oh = out_h as usize;
    let fwi = frame.width as usize;
    let fhi = frame.height as usize;
    let src = &frame.rgba;
    let mut out = vec![0u8; ow * oh * 4];

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let rows_per = oh.div_ceil(threads);

    std::thread::scope(|sc| {
        for (ci, chunk) in out.chunks_mut(rows_per * ow * 4).enumerate() {
            let y0 = ci * rows_per;
            let src = &*src;
            sc.spawn(move || {
                let rows = chunk.len() / (ow * 4);
                for r in 0..rows {
                    let j = y0 + r;
                    let oy = 2.0 * j as f32 / (oh as f32 - 1.0) - 1.0;
                    for i in 0..ow {
                        let ox = 2.0 * i as f32 / (ow as f32 - 1.0) - 1.0;
                        // Inverse rotation R(-angle) to find the pre-rotation coord.
                        let rx = ox * cos + oy * sin;
                        let ry = -ox * sin + oy * cos;
                        let r2 = rx * rx + ry * ry;
                        let f = 1.0 + k1 * r2 + k2 * r2 * r2;
                        let sx = (0.5 + 0.5 * rx * f).clamp(0.0, 1.0);
                        let sy = (0.5 + 0.5 * ry * f).clamp(0.0, 1.0);
                        let px = (crop_min_x + sx * crop_w) as usize;
                        let py = (crop_min_y + sy * crop_h) as usize;
                        let si = (py.min(fhi - 1) * fwi + px.min(fwi - 1)) * 4;
                        let o = (r * ow + i) * 4;
                        chunk[o] = src[si + 2]; // B
                        chunk[o + 1] = src[si + 1]; // G
                        chunk[o + 2] = src[si]; // R
                        chunk[o + 3] = 255;
                    }
                }
            });
        }
    });
    out
}
