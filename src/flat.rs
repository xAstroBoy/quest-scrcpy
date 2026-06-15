//! "Flat view" source: the whole undistorted Quest view (home environment +
//! panels), captured on-device by the embedded `FlatStream` agent and piped to
//! us over `adb exec-out` as H.264. No client-side lens de-warp — the device
//! composites it flat for us (the casting view).
//!
//! Wire format from the agent: `[u32 w][u32 h]` then repeated `[u32 len][AnnexB AU]`.

use crate::adb;
use crate::decoder::{Frame, H264Decoder};
use crate::stream::{FrameSlot, Status};
use anyhow::{Context, Result, anyhow, bail};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The on-device agent, baked into the binary (built from `agent/` — see
/// `agent/src/com/questflat/FlatStream.java`). Pushed and run like scrcpy's jar.
const AGENT: &[u8] = include_bytes!("../assets/quest-flat-agent.jar");
const AGENT_DEVICE_PATH: &str = "/data/local/tmp/quest-flat-agent.jar";
const AGENT_MAIN: &str = "com.questflat.FlatStream";

#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;

fn adb_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new(adb::adb_path());
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(NO_WINDOW);
    }
    cmd
}

/// Write the embedded agent jar to the device. Must precede each agent launch.
fn push_agent(serial: &str) -> Result<()> {
    let mut tmp = std::env::temp_dir();
    tmp.push("quest-flat-agent.jar");
    std::fs::write(&tmp, AGENT).with_context(|| "writing temp flat agent")?;
    let tmp_str = tmp.to_string_lossy().to_string();
    let status = adb_cmd(&["-s", serial, "push", &tmp_str, AGENT_DEVICE_PATH])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("adb push flat agent failed")?;
    if !status.success() {
        bail!("adb push flat agent returned {status}");
    }
    Ok(())
}

/// Launch the agent over `adb exec-out` (binary stdout = the H.264 stream).
fn spawn_agent(serial: &str, w: u32, h: u32, bitrate: u32, fps: u32) -> Result<Child> {
    let shell = format!(
        "CLASSPATH={AGENT_DEVICE_PATH} app_process /system/bin {AGENT_MAIN} -w {w} -h {h} -b {bitrate} -f {fps}"
    );
    adb_cmd(&["-s", serial, "exec-out", &shell])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn flat agent via adb exec-out")
}

/// Forward the agent's stderr (its logs) to ours for diagnosis.
fn drain_stderr(child: &mut Child) {
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[flat-agent] {line}");
            }
        });
    }
}

/// Skip any stdout preamble (ART/linker warnings) up to the `QFLT` magic, then
/// read the 8-byte `[w][h]` header.
fn read_dims<R: Read>(r: &mut R) -> Result<(u32, u32)> {
    let magic = b"QFLT";
    let mut window = [0u8; 4];
    let mut scanned = 0u32;
    loop {
        let mut byte = [0u8; 1];
        r.read_exact(&mut byte).context("scanning for flat stream magic")?;
        window.rotate_left(1);
        window[3] = byte[0];
        scanned += 1;
        if &window == magic {
            break;
        }
        if scanned > 65536 {
            bail!("no QFLT magic found in agent stdout preamble");
        }
    }
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr).context("reading flat stream header")?;
    let w = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let h = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        bail!("implausible flat stream dims {w}x{h}");
    }
    Ok((w, h))
}

/// Read one `[u8 kind][u32 len][data]` packet. kind: 0 = video access unit,
/// 1 = audio AAC frame, 2 = audio AAC codec config. None on clean EOF.
fn read_packet<R: Read>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0u8; 1];
    match r.read_exact(&mut kind) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut lenbuf = [0u8; 4];
    r.read_exact(&mut lenbuf).context("reading packet length")?;
    let len = u32::from_be_bytes(lenbuf) as usize;
    if len == 0 || len > 64_000_000 {
        bail!("bad packet length {len}");
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data).context("reading packet")?;
    Ok(Some((kind[0], data)))
}

/// Headless: capture the live flat view for `seconds`, save the latest frame to
/// PNG. Proves the whole pipeline (agent -> exec-out -> decode) end to end.
pub fn run_flat_shot(
    serial: &str,
    out: PathBuf,
    seconds: u64,
    w: u32,
    h: u32,
    bitrate: u32,
    fps: u32,
) -> Result<()> {
    push_agent(serial)?;
    let mut child = spawn_agent(serial, w, h, bitrate, fps)?;
    drain_stderr(&mut child);
    let mut stdout = child.stdout.take().context("agent stdout missing")?;

    let (w, h) = read_dims(&mut stdout)?;
    eprintln!("[flat] stream {w}x{h}, decoding for {seconds}s…");
    let mut dec = H264Decoder::new(w, h)?;

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last: Option<Frame> = None;
    let mut count = 0u64;
    while Instant::now() < deadline {
        match read_packet(&mut stdout)? {
            Some((0, au)) => {
                for frame in dec.decode(&au)? {
                    last = Some(frame);
                    count += 1;
                }
            }
            Some(_) => {} // headless: ignore audio
            None => break,
        }
    }
    let _ = child.kill();

    let frame = last.ok_or_else(|| anyhow!("no frames decoded"))?;
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
        .ok_or_else(|| anyhow!("frame buffer size mismatch"))?;
    img.save(&out)?;
    eprintln!("[flat] decoded {count} frames; saved {} ({}x{})", out.display(), frame.width, frame.height);
    Ok(())
}

/// A live flat-view session for the GUI: owns the agent + decode thread and
/// publishes the latest frame into a [`FrameSlot`] (same as the scrcpy path).
pub struct FlatHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
    pub slot: Arc<Mutex<FrameSlot>>,
    pub status: Arc<Mutex<Status>>,
}

impl FlatHandle {
    pub fn start(
        serial: String,
        w: u32,
        h: u32,
        bitrate: u32,
        fps: u32,
        repaint: egui::Context,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(FrameSlot::default()));
        let status = Arc::new(Mutex::new(Status::Connecting));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

        let join = {
            let (stop, slot, status, child, serial) =
                (stop.clone(), slot.clone(), status.clone(), child.clone(), serial.clone());
            std::thread::Builder::new()
                .name("flat-stream".into())
                .spawn(move || {
                    if let Err(e) = run_gui(&serial, w, h, bitrate, fps, &stop, &slot, &status, &child, &repaint) {
                        if !stop.load(Ordering::Relaxed) {
                            *status.lock().unwrap() = Status::Error(format!("{e:#}"));
                            repaint.request_repaint();
                        }
                    }
                    if let Some(c) = child.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                    if !stop.load(Ordering::Relaxed) {
                        *status.lock().unwrap() = Status::Stopped;
                    }
                })
                .expect("spawn flat thread")
        };

        Self { stop, join: Some(join), child, slot, status }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

impl Drop for FlatHandle {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(j) = self.join.take() {
            std::thread::spawn(move || {
                let _ = j.join();
            });
        }
    }
}

fn run_gui(
    serial: &str,
    w: u32,
    h: u32,
    bitrate: u32,
    fps: u32,
    stop: &Arc<AtomicBool>,
    slot: &Arc<Mutex<FrameSlot>>,
    status: &Arc<Mutex<Status>>,
    child_slot: &Arc<Mutex<Option<Child>>>,
    repaint: &egui::Context,
) -> Result<()> {
    push_agent(serial)?;
    let mut child = spawn_agent(serial, w, h, bitrate, fps)?;
    drain_stderr(&mut child);
    let mut stdout = child.stdout.take().context("agent stdout missing")?;
    *child_slot.lock().unwrap() = Some(child);

    let (w, h) = read_dims(&mut stdout)?;
    let mut dec = H264Decoder::new(w, h)?;
    *status.lock().unwrap() = Status::Streaming { width: w, height: h, fps: 0.0 };
    repaint.request_repaint();

    // Audio plays through the same AAC player the scrcpy path uses; created
    // lazily on the first audio packet (the agent sends config first).
    let mut audio: Option<crate::audioplay::AacPlayer> = None;
    let mut frames_since = 0u32;
    let mut last_tick = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let (kind, data) = match read_packet(&mut stdout) {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                return Err(e);
            }
        };
        if kind != 0 {
            // 1 = AAC frame, 2 = AAC codec config.
            if audio.is_none() {
                audio = crate::audioplay::AacPlayer::new()
                    .map_err(|e| eprintln!("[flat] audio init failed: {e:#}"))
                    .ok();
            }
            if let Some(p) = audio.as_mut() {
                if let Err(e) = p.feed(&data, kind == 2) {
                    eprintln!("[flat] audio feed error: {e:#}");
                }
            }
            continue;
        }
        for frame in dec.decode(&data)? {
            let (fw, fh) = (frame.width, frame.height);
            {
                let mut s = slot.lock().unwrap();
                s.frame = Some(frame);
                s.generation = s.generation.wrapping_add(1);
            }
            frames_since += 1;
            repaint.request_repaint();
            if last_tick.elapsed() >= Duration::from_millis(500) {
                let fps = frames_since as f32 / last_tick.elapsed().as_secs_f32();
                frames_since = 0;
                last_tick = Instant::now();
                *status.lock().unwrap() = Status::Streaming { width: fw, height: fh, fps };
            }
        }
    }
    Ok(())
}
