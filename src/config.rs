//! A tiny `key=value` settings file kept next to the executable, so the lens
//! correction, tilt, crop and quality knobs persist across launches. Hand-rolled
//! to avoid pulling in a serialization crate (keeps the build all-in-one).

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub lens_correct: bool,
    pub lens_k1: f32,
    pub lens_k2: f32,
    pub rotation_deg: f32,
    /// Crop window: [min_x, min_y, max_x, max_y] in normalised texture coords.
    pub uv: [f32; 4],
    pub display_id: u32,
    pub max_size: u32,
    pub bitrate_mbps: u32,
    pub max_fps: u32,
    pub audio: bool,
    /// Remembered wireless-adb addresses (ip:port) for quick reconnect.
    pub remotes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Default to the dialed-in Quest 3 view so it looks right out of the box.
            lens_correct: true,
            lens_k1: 0.02,
            lens_k2: -0.20,
            rotation_deg: -1.0,
            uv: [0.0, 0.0, 0.5, 1.0],
            display_id: 0,
            max_size: 1920,
            bitrate_mbps: 16,
            max_fps: 0,
            audio: false,
            remotes: Vec::new(),
        }
    }
}

fn config_path() -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("quest-scrcpy.cfg")
}

impl Config {
    pub fn load() -> Option<Config> {
        let text = std::fs::read_to_string(config_path()).ok()?;
        let mut c = Config::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "lens_correct" => c.lens_correct = v == "true",
                "lens_k1" => c.lens_k1 = v.parse().unwrap_or(c.lens_k1),
                "lens_k2" => c.lens_k2 = v.parse().unwrap_or(c.lens_k2),
                "rotation_deg" => c.rotation_deg = v.parse().unwrap_or(c.rotation_deg),
                "uv" => {
                    let nums: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                    if nums.len() == 4 {
                        c.uv = [nums[0], nums[1], nums[2], nums[3]];
                    }
                }
                "display_id" => c.display_id = v.parse().unwrap_or(c.display_id),
                "max_size" => c.max_size = v.parse().unwrap_or(c.max_size),
                "bitrate_mbps" => c.bitrate_mbps = v.parse().unwrap_or(c.bitrate_mbps),
                "max_fps" => c.max_fps = v.parse().unwrap_or(c.max_fps),
                "audio" => c.audio = v == "true",
                "remote" => {
                    let r = v.to_string();
                    if !r.is_empty() && !c.remotes.contains(&r) {
                        c.remotes.push(r);
                    }
                }
                _ => {}
            }
        }
        Some(c)
    }

    pub fn save(&self) {
        let body = format!(
            "# quest-scrcpy settings (auto-saved)\n\
             lens_correct={}\n\
             lens_k1={}\n\
             lens_k2={}\n\
             rotation_deg={}\n\
             uv={},{},{},{}\n\
             display_id={}\n\
             max_size={}\n\
             bitrate_mbps={}\n\
             max_fps={}\n\
             audio={}\n",
            self.lens_correct,
            self.lens_k1,
            self.lens_k2,
            self.rotation_deg,
            self.uv[0],
            self.uv[1],
            self.uv[2],
            self.uv[3],
            self.display_id,
            self.max_size,
            self.bitrate_mbps,
            self.max_fps,
            self.audio,
        );
        let mut body = body;
        for r in &self.remotes {
            body.push_str(&format!("remote={r}\n"));
        }
        let _ = std::fs::write(config_path(), body);
    }
}
