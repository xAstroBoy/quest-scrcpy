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

/// Forward an ffmpeg child's stderr to our own stderr (tagged), on a thread, so
/// the pipe never fills and the messages are visible when something goes wrong.
pub fn drain_stderr(child: &mut Child, tag: &'static str) {
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    eprintln!("[ffmpeg/{tag}] {line}");
                }
            }
        });
    }
}
