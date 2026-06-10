//! Thin wrapper over the `adb` CLI plus the bits of the scrcpy-server launch
//! protocol that live on the host side (push, forward, start, list-displays).

use anyhow::{Context, Result, anyhow, bail};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// Serializes server pushes so two threads never `adb push` the same path at
/// once (which would corrupt the jar). The scrcpy server deletes its own jar on
/// startup, so we must re-push before *every* server launch — never cache it.
static PUSH_LOCK: Mutex<()> = Mutex::new(());

/// scrcpy-server protocol version this client speaks. Must match the embedded
/// jar exactly — the server rejects a version mismatch. v4.0 carries the Quest
/// fix (no SurfaceControl fallback that black-screens on Horizon OS 74+).
pub const SCRCPY_VERSION: &str = "4.0";

/// The scrcpy-server jar, baked straight into the binary so we depend on nothing
/// from SideQuest at runtime.
const SCRCPY_SERVER: &[u8] = include_bytes!("../assets/scrcpy-server.jar");

const DEVICE_JAR_PATH: &str = "/data/local/tmp/scrcpy-server.jar";

/// `CREATE_NO_WINDOW` — keeps adb from flashing console windows when we are a GUI.
#[cfg(windows)]
const NO_WINDOW: u32 = 0x0800_0000;

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(adb_path());
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(NO_WINDOW);
    }
    cmd
}

/// Resolve the adb executable: honour `ADB` / PATH, else fall back to the
/// platform-tools location this machine is known to have.
pub fn adb_path() -> PathBuf {
    if let Ok(p) = std::env::var("ADB") {
        return PathBuf::from(p);
    }
    PathBuf::from("adb")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub model: String,
    pub state: String,
    pub usb: bool,
}

impl Device {
    /// A friendly label for dropdowns, e.g. `Quest_3 (USB)` or `Quest_3 (192.168.1.35:5555)`.
    pub fn label(&self) -> String {
        let model = if self.model.is_empty() { "device" } else { &self.model };
        if self.usb {
            format!("{model}  (USB · {})", self.serial)
        } else {
            format!("{model}  ({})", self.serial)
        }
    }
}

/// `adb devices -l`, parsed.
pub fn list_devices() -> Result<Vec<Device>> {
    let out = command(&["devices", "-l"])
        .output()
        .context("failed to run `adb devices` — is adb on PATH?")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devices = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let serial = match parts.next() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let state = parts.next().unwrap_or("").to_string();
        let mut model = String::new();
        for tok in parts {
            if let Some(m) = tok.strip_prefix("model:") {
                model = m.to_string();
            }
        }
        // Network devices look like 192.168.x.x:5555.
        let usb = !serial.contains(':');
        devices.push(Device { serial, model, state, usb });
    }
    Ok(devices)
}

/// Write the embedded server jar to a temp file and push it to the device.
/// Must be called immediately before each [`start_server`]/[`list_displays`],
/// because the server unlinks the jar as it starts.
pub fn push_server(serial: &str) -> Result<()> {
    // Serialize: two simultaneous pushes to the same device path corrupt it.
    let _guard = PUSH_LOCK.lock().unwrap();

    let mut tmp = std::env::temp_dir();
    tmp.push("quest-scrcpy-server.jar");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating temp server at {}", tmp.display()))?;
        f.write_all(SCRCPY_SERVER)?;
    }
    let tmp_str = tmp.to_string_lossy().to_string();
    let status = command(&["-s", serial, "push", &tmp_str, DEVICE_JAR_PATH])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("adb push scrcpy-server failed")?;
    if !status.success() {
        bail!("adb push scrcpy-server returned {status}");
    }
    Ok(())
}

/// Normalise a user-typed address: add the default wireless-adb port if absent.
pub fn normalize_addr(addr: &str) -> String {
    let addr = addr.trim();
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:5555")
    }
}

/// `adb connect <ip:port>`. Returns adb's status line on success.
pub fn connect(addr: &str) -> Result<String> {
    let addr = normalize_addr(addr);
    let out = command(&["connect", &addr])
        .output()
        .context("failed to run `adb connect`")?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    let low = text.to_lowercase();
    if low.contains("fail") || low.contains("cannot") || low.contains("unable") || low.contains("refused") {
        bail!("{text}");
    }
    Ok(if text.is_empty() { format!("connected to {addr}") } else { text })
}

/// `adb disconnect <ip:port>`. Best-effort.
pub fn disconnect(addr: &str) {
    let addr = normalize_addr(addr);
    let _ = command(&["disconnect", &addr])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `adb forward tcp:<port> localabstract:scrcpy`. Returns the chosen local port.
pub fn forward(serial: &str, port: u16) -> Result<u16> {
    let local = format!("tcp:{port}");
    let remote = "localabstract:scrcpy";
    let status = command(&["-s", serial, "forward", &local, remote])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("adb forward failed")?;
    if !status.success() {
        bail!("adb forward returned {status}");
    }
    Ok(port)
}

/// Tear down a forward created with [`forward`]. Best-effort.
pub fn remove_forward(serial: &str, port: u16) {
    let local = format!("tcp:{port}");
    let _ = command(&["-s", serial, "forward", "--remove", &local])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Options handed to the scrcpy-server. Mirrors the subset of v2.0 keys we use.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub display_id: u32,
    pub max_size: u32,
    pub video_bit_rate: u32,
    pub max_fps: u32,
    pub audio: bool,
    pub audio_bit_rate: u32,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            display_id: 0,
            max_size: 0, // 0 = no downscale (full panel)
            video_bit_rate: 16_000_000,
            max_fps: 0,
            audio: false,
            audio_bit_rate: 128_000,
        }
    }
}

impl ServerOptions {
    fn to_args(&self) -> Vec<String> {
        let mut a = vec![
            SCRCPY_VERSION.to_string(),
            "log_level=info".into(),
            "tunnel_forward=true".into(),
            "control=false".into(),
            "cleanup=true".into(),
            "send_device_meta=true".into(),
            "send_frame_meta=true".into(),
            "send_stream_meta=true".into(),
            "send_dummy_byte=true".into(),
            "video_codec=h264".into(),
            format!("display_id={}", self.display_id),
            format!("max_size={}", self.max_size),
            format!("video_bit_rate={}", self.video_bit_rate),
            format!("max_fps={}", self.max_fps),
        ];
        if self.audio {
            // AAC so we can reuse the Media Foundation decode stack (no libopus build).
            a.push("audio=true".into());
            a.push("audio_codec=aac".into());
            a.push(format!("audio_bit_rate={}", self.audio_bit_rate));
        } else {
            a.push("audio=false".into());
        }
        a
    }
}

/// Spawn the scrcpy-server `app_process`. The returned [`Child`] keeps the stream
/// alive; dropping/killing it stops mirroring on the device.
pub fn start_server(serial: &str, opts: &ServerOptions) -> Result<Child> {
    let server_args = opts.to_args().join(" ");
    let shell_cmd = format!(
        "CLASSPATH={DEVICE_JAR_PATH} app_process / com.genymobile.scrcpy.Server {server_args}"
    );
    let child = command(&["-s", serial, "shell", &shell_cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn scrcpy-server via adb shell")?;
    Ok(child)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

impl DisplayInfo {
    pub fn label(&self) -> String {
        format!("Display {} — {}x{}", self.id, self.width, self.height)
    }
}

/// Run the server with `list_displays=true`, parse the `--display=ID (WxH)` lines.
pub fn list_displays(serial: &str) -> Result<Vec<DisplayInfo>> {
    push_server(serial)?;
    let shell_cmd = format!(
        "CLASSPATH={DEVICE_JAR_PATH} app_process / com.genymobile.scrcpy.Server {ver} \
         log_level=info list_displays=true",
        ver = SCRCPY_VERSION
    );
    let out = command(&["-s", serial, "shell", &shell_cmd])
        .output()
        .context("failed to query displays")?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let displays = parse_displays(&text);
    if displays.is_empty() {
        return Err(anyhow!(
            "no displays parsed from server output:\n{}",
            text.trim()
        ));
    }
    Ok(displays)
}

fn parse_displays(text: &str) -> Vec<DisplayInfo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // v4.0: "--display-id=0    (4128x2208)"; v2.0 used "--display=0 …".
        let Some(rest) = line
            .strip_prefix("--display-id=")
            .or_else(|| line.strip_prefix("--display="))
        else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let Some(id_str) = it.next() else { continue };
        let Ok(id) = id_str.parse::<u32>() else { continue };
        let Some(dim) = it.next() else { continue };
        let dim = dim.trim_start_matches('(').trim_end_matches(')');
        let Some((w, h)) = dim.split_once('x') else { continue };
        if let (Ok(width), Ok(height)) = (w.parse(), h.parse()) {
            out.push(DisplayInfo { id, width, height });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_display_lines() {
        let sample = "\
[server] INFO: List of displays:
    --display-id=0    (4128x2208)
    --display-id=9    (4128x2208)
    --display-id=11    (1020x125)
";
        let d = parse_displays(sample);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0], DisplayInfo { id: 0, width: 4128, height: 2208 });
        assert_eq!(d[1].id, 9);
        assert_eq!(d[2], DisplayInfo { id: 11, width: 1020, height: 125 });
    }
}
