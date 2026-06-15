//! Locating and launching the optional `ffmpeg` CLI.
//!
//! When `ffmpeg` is on `PATH` (or pointed at by `$FFMPEG`) we use it for H.264
//! decode, AAC playback and recording. This is what lets the client run on
//! Linux and macOS, where the Media Foundation backend does not exist — and it
//! is used on Windows too when present (it tends to be more robust), otherwise
//! we fall back to the built-in MF codecs.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

/// `CREATE_NO_WINDOW` — keep spawned ffmpeg from flashing a console window.
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;

fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(NO_WINDOW);
    }
    let _ = cmd;
}

fn probe(candidate: &str) -> Option<PathBuf> {
    let mut cmd = Command::new(candidate);
    cmd.arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_window(&mut cmd);
    match cmd.status() {
        Ok(s) if s.success() => Some(PathBuf::from(candidate)),
        _ => None,
    }
}

/// The resolved ffmpeg executable, if one is usable. Probed once and cached.
pub fn path() -> Option<&'static PathBuf> {
    static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
    FFMPEG
        .get_or_init(|| {
            if let Ok(p) = std::env::var("FFMPEG") {
                if let Some(found) = probe(&p) {
                    return Some(found);
                }
            }
            probe("ffmpeg")
        })
        .as_ref()
}

/// Is an ffmpeg CLI available to use as the decode/record/audio backend?
pub fn available() -> bool {
    path().is_some()
}

/// Start an ffmpeg command with quiet logging and no console-window flash.
/// `None` if ffmpeg isn't available.
pub fn command() -> Option<Command> {
    let exe = path()?;
    let mut cmd = Command::new(exe);
    cmd.arg("-hide_banner").args(["-loglevel", "error"]);
    hide_window(&mut cmd);
    Some(cmd)
}

/// ADTS framing parameters parsed from an AudioSpecificConfig. The agent/scrcpy
/// send raw AAC-LC (an ASC then bare access units); ffmpeg's CLI wants ADTS, so
/// both the player and the recorder prepend a 7-byte ADTS header per access unit.
#[derive(Clone, Copy)]
pub struct Adts {
    profile: u8,  // ADTS 2-bit profile = audioObjectType - 1
    freq_idx: u8, // sampling frequency index (4 bits)
    channels: u8, // channel configuration (3 bits in ADTS)
}

impl Adts {
    pub fn from_asc(asc: &[u8]) -> Self {
        if asc.len() < 2 {
            return Adts { profile: 1, freq_idx: 4, channels: 2 }; // AAC-LC, 44.1k, stereo
        }
        let obj = (asc[0] >> 3) & 0x1F;
        let freq_idx = (((asc[0] & 0x07) << 1) | (asc[1] >> 7)) & 0x0F;
        let chan = (asc[1] >> 3) & 0x0F;
        Adts { profile: obj.saturating_sub(1) & 0x03, freq_idx, channels: chan.clamp(1, 7) }
    }

    /// The 7-byte ADTS header for an access unit of `payload_len` bytes.
    pub fn header(&self, payload_len: usize) -> [u8; 7] {
        let frame_len = (7 + payload_len) as u32;
        let mut h = [0u8; 7];
        h[0] = 0xFF;
        h[1] = 0xF1; // syncword + MPEG-4 + Layer 0 + no CRC
        h[2] = (self.profile << 6) | ((self.freq_idx & 0x0F) << 2) | ((self.channels >> 2) & 0x01);
        h[3] = ((self.channels & 0x03) << 6) | (((frame_len >> 11) & 0x03) as u8);
        h[4] = ((frame_len >> 3) & 0xFF) as u8;
        h[5] = (((frame_len & 0x07) << 5) as u8) | 0x1F;
        h[6] = 0xFC;
        h
    }
}

/// Where ffmpeg backend stderr is mirrored, so problems are diagnosable even in
/// the release GUI (which has no console). Honors `$QUEST_SCRCPY_FFMPEG_LOG`.
pub fn log_path() -> PathBuf {
    if let Ok(p) = std::env::var("QUEST_SCRCPY_FFMPEG_LOG") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("quest-scrcpy-ffmpeg.log")
}

/// Forward an ffmpeg child's stderr to our stderr (tagged) AND a log file, on a
/// thread, so the pipe never fills and the messages survive a windowed build.
pub fn drain_stderr(child: &mut Child, tag: &'static str) {
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path())
                .ok();
            let reader = std::io::BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    eprintln!("[ffmpeg/{tag}] {line}");
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "[{tag}] {line}");
                    }
                }
            }
        });
    }
}
