//! egui front-end: pick a device/display, tune quality, toggle audio, and watch
//! the live mirror with a draggable/zoomable crop (ideal for the Quest's small
//! floating panel — drag to pan, scroll to zoom, or snap to an eye preset).

use crate::adb::{self, Device, DisplayInfo};
use crate::config::Config;
use crate::decoder::Frame;
use crate::stream::{Status, StreamConfig, StreamHandle, ViewParams};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Sense, pos2, vec2};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The "solid" Quest 3 view profile applied by the one-click preset button.
/// Left-eye crop + lens-flatten coefficients + tilt. Tune to taste; the GUI also
/// remembers your last manual values via the config file.
const QUEST3_CROP: [f32; 4] = [0.0, 0.0, 0.5, 1.0];
const QUEST3_K1: f32 = 0.02;
const QUEST3_K2: f32 = -0.20;
const QUEST3_TILT: f32 = -1.0;

/// Quality knobs that require a (re)connect to take effect.
#[derive(Clone, PartialEq)]
struct Settings {
    display_id: u32,
    max_size: u32,
    bitrate_mbps: u32,
    max_fps: u32,
    audio: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { display_id: 0, max_size: 1920, bitrate_mbps: 16, max_fps: 0, audio: false }
    }
}

type DisplayFetch = Arc<Mutex<Option<anyhow::Result<Vec<DisplayInfo>>>>>;

/// Optional overrides coming from the CLI `mirror` subcommand.
#[derive(Default)]
pub struct StartupConfig {
    pub serial: Option<String>,
    pub display_id: Option<u32>,
    pub max_size: Option<u32>,
    pub bitrate_mbps: Option<u32>,
    pub max_fps: Option<u32>,
    pub audio: Option<bool>,
    pub autostart: bool,
}

pub struct App {
    devices: Vec<Device>,
    selected_serial: Option<String>,
    displays: Vec<DisplayInfo>,
    display_fetch: DisplayFetch,
    settings: Settings,

    stream: Option<StreamHandle>,
    /// Live "flat view": the whole undistorted Quest view via the unrooted agent.
    flat: Option<crate::flat::FlatHandle>,
    texture: Option<egui::TextureHandle>,
    last_generation: u64,
    tex_size: [usize; 2],
    /// Newest decoded frame, kept so we can screenshot the current view.
    last_frame: Option<Frame>,
    /// Transient feedback for capture actions (screenshot/record paths).
    capture_msg: Option<String>,

    /// Crop window in normalised texture coordinates (0..1).
    uv: Rect,

    /// Flatten the Quest's lens (radial) distortion into a rectilinear 2D image.
    lens_correct: bool,
    /// Radial distortion coefficients (k1 quadratic, k2 quartic).
    lens_k1: f32,
    lens_k2: f32,
    /// Extra in-plane rotation to straighten residual tilt (degrees).
    rotation_deg: f32,

    want_autostart: bool,
    /// A display query is requested but waits until it's safe to run (the list
    /// server would otherwise delete the jar out from under a connecting stream).
    need_display_fetch: bool,
    fetch_inflight: bool,

    /// Wireless-adb: the ip:port being typed, remembered addresses, and the
    /// result of an in-flight `adb connect` running on a background thread.
    remote_input: String,
    remotes: Vec<String>,
    connect_result: Arc<Mutex<Option<(String, Result<String, String>)>>>,
    connecting: bool,

    /// Last-persisted settings + a debounce timer, so edits auto-save shortly
    /// after the user stops fiddling (rather than thrashing the disk each frame).
    persisted: Config,
    dirty_since: Option<Instant>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: StartupConfig) -> Self {
        // egui's bundled font has no emoji glyphs, so load a system font that does
        // (Segoe UI Emoji / Symbol) — otherwise 🥽🔊📷 etc. render as tofu boxes.
        install_emoji_font(&cc.egui_ctx);
        // Saved settings are the baseline; explicit CLI flags override quality knobs.
        let cfg = Config::load().unwrap_or_default();
        let mut settings = Settings {
            display_id: cfg.display_id,
            max_size: cfg.max_size,
            bitrate_mbps: cfg.bitrate_mbps,
            max_fps: cfg.max_fps,
            audio: cfg.audio,
        };
        if let Some(v) = startup.display_id {
            settings.display_id = v;
        }
        if let Some(v) = startup.max_size {
            settings.max_size = v;
        }
        if let Some(v) = startup.bitrate_mbps {
            settings.bitrate_mbps = v;
        }
        if let Some(v) = startup.max_fps {
            settings.max_fps = v;
        }
        if let Some(v) = startup.audio {
            settings.audio = v;
        }

        let mut app = Self {
            devices: Vec::new(),
            selected_serial: None,
            displays: Vec::new(),
            display_fetch: Arc::new(Mutex::new(None)),
            settings,
            stream: None,
            flat: None,
            texture: None,
            last_generation: 0,
            tex_size: [0, 0],
            last_frame: None,
            capture_msg: None,
            uv: Rect::from_min_max(
                pos2(cfg.uv[0], cfg.uv[1]),
                pos2(cfg.uv[2], cfg.uv[3]),
            ),
            lens_correct: cfg.lens_correct,
            lens_k1: cfg.lens_k1,
            lens_k2: cfg.lens_k2,
            rotation_deg: cfg.rotation_deg,
            want_autostart: startup.autostart,
            need_display_fetch: false,
            fetch_inflight: false,
            remote_input: String::new(),
            remotes: cfg.remotes.clone(),
            connect_result: Arc::new(Mutex::new(None)),
            connecting: false,
            persisted: cfg,
            dirty_since: None,
        };
        // Snapshot what we actually ended up with (after CLI overrides) so we
        // don't immediately re-save on first frame.
        app.persisted = app.current_config();
        app.refresh_devices();
        // An explicit --serial overrides auto-pick.
        if let Some(s) = startup.serial {
            if app.devices.iter().any(|d| d.serial == s) {
                app.selected_serial = Some(s);
                app.request_display_fetch();
            }
        }
        app
    }

    fn refresh_devices(&mut self) {
        let prev = self.selected_serial.clone();
        self.devices = adb::list_devices().unwrap_or_default();
        let online: Vec<&Device> =
            self.devices.iter().filter(|d| d.state == "device").collect();

        // Keep the current selection if it's still around, else prefer USB.
        let still_present = prev
            .as_ref()
            .map(|s| online.iter().any(|d| &d.serial == s))
            .unwrap_or(false);
        if !still_present {
            self.selected_serial = online
                .iter()
                .find(|d| d.usb)
                .or_else(|| online.first())
                .map(|d| d.serial.clone());
            self.request_display_fetch();
        }
    }

    /// Request a display query; the actual (jar-consuming) list server runs later
    /// from [`drive_display_fetch`], once no stream is mid-connect.
    fn request_display_fetch(&mut self) {
        self.displays.clear();
        self.need_display_fetch = true;
    }

    /// Running a list server deletes the jar; it's only safe when nothing is
    /// connecting (idle, or already streaming/stopped).
    fn fetch_safe(&self) -> bool {
        match self.stream.as_ref().map(|s| s.status()) {
            None | Some(Status::Streaming { .. }) | Some(Status::Error(_)) | Some(Status::Stopped) => {
                true
            }
            Some(Status::Connecting) => false,
        }
    }

    fn drive_display_fetch(&mut self) {
        if !self.need_display_fetch || self.fetch_inflight || !self.fetch_safe() {
            return;
        }
        let Some(serial) = self.selected_serial.clone() else {
            self.need_display_fetch = false;
            return;
        };
        self.need_display_fetch = false;
        self.fetch_inflight = true;
        let slot = self.display_fetch.clone();
        *slot.lock().unwrap() = None;
        std::thread::spawn(move || {
            let res = adb::list_displays(&serial);
            *slot.lock().unwrap() = Some(res);
        });
    }

    fn poll_displays(&mut self) {
        let taken = self.display_fetch.lock().unwrap().take();
        if let Some(res) = taken {
            self.fetch_inflight = false;
            match res {
                Ok(mut list) => {
                    // Show every real display so the user can pick (Quest exposes
                    // several 4128x2208 panels — e.g. id 0 vs 9 — and which one
                    // mirrors correctly varies). Only drop the 1x1 sentinels.
                    list.retain(|d| d.width >= 2 && d.height >= 2);
                    // Biggest first: the real headset panels sort to the top.
                    list.sort_by(|a, b| (b.width * b.height).cmp(&(a.width * a.height)));
                    self.displays = list;
                    // Keep the current display if still valid; otherwise prefer 0.
                    if !self.displays.iter().any(|d| d.id == self.settings.display_id) {
                        if self.displays.iter().any(|d| d.id == 0) {
                            self.settings.display_id = 0;
                        } else if let Some(first) = self.displays.first() {
                            self.settings.display_id = first.id;
                        }
                    }
                }
                Err(_) => self.displays.clear(),
            }
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let Some(serial) = self.selected_serial.clone() else { return };
        self.stream = None; // drop & stop any existing session first
        self.texture = None;
        self.last_frame = None;
        self.last_generation = 0;
        let cfg = StreamConfig {
            serial,
            display_id: self.settings.display_id,
            max_size: self.settings.max_size,
            video_bit_rate: self.settings.bitrate_mbps * 1_000_000,
            max_fps: self.settings.max_fps,
            audio: self.settings.audio,
            audio_bit_rate: 128_000,
        };
        self.stream = Some(StreamHandle::start(cfg, ctx.clone()));
    }

    fn disconnect(&mut self) {
        self.stream = None;
        self.flat = None;
        self.texture = None;
        self.last_frame = None;
    }

    /// Start the live flat-view source: the whole undistorted Quest view (home
    /// environment + panels) captured by the unrooted on-device agent. No lens
    /// de-warp or crop — the device composites it flat for us.
    fn connect_flat(&mut self, ctx: &egui::Context) {
        let Some(serial) = self.selected_serial.clone() else { return };
        self.stream = None;
        self.flat = None;
        self.texture = None;
        self.last_frame = None;
        self.last_generation = 0;
        // The flat view is already undistorted, full-frame and level — no crop,
        // no lens de-warp, and crucially no tilt (else we'd rotate a flat image).
        self.lens_correct = false;
        self.rotation_deg = 0.0;
        self.uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        // Resolution / bitrate / fps from the quality knobs. We pass a *square*
        // bounding box (max_size × max_size); the agent fits the real flat-view
        // aspect inside it, so the longer side reaches max_size whatever the
        // shape — no forced 16:9 that would letterbox/crop the view.
        let cfg = self.flat_config();
        self.flat = Some(crate::flat::FlatHandle::start(
            serial,
            cfg.max_size,
            cfg.max_size, // square bounding box; the agent fits the real aspect in it
            cfg.bitrate,
            cfg.fps,
            cfg.audio,
            ctx.clone(),
        ));
    }

    /// Apply the baked-in "good Quest 3 view" in one click.
    fn apply_quest3_preset(&mut self) {
        self.uv = Rect::from_min_max(pos2(QUEST3_CROP[0], QUEST3_CROP[1]), pos2(QUEST3_CROP[2], QUEST3_CROP[3]));
        self.lens_correct = true;
        self.lens_k1 = QUEST3_K1;
        self.lens_k2 = QUEST3_K2;
        self.rotation_deg = QUEST3_TILT;
    }

    /// Snapshot the persistable settings from the current UI state.
    fn current_config(&self) -> Config {
        Config {
            lens_correct: self.lens_correct,
            lens_k1: self.lens_k1,
            lens_k2: self.lens_k2,
            rotation_deg: self.rotation_deg,
            uv: [self.uv.min.x, self.uv.min.y, self.uv.max.x, self.uv.max.y],
            display_id: self.settings.display_id,
            max_size: self.settings.max_size,
            bitrate_mbps: self.settings.bitrate_mbps,
            max_fps: self.settings.max_fps,
            audio: self.settings.audio,
            remotes: self.remotes.clone(),
        }
    }

    /// Kick off `adb connect <addr>` on a background thread (network calls can
    /// hang on a bad address, so never run them on the UI thread).
    fn start_remote_connect(&mut self, addr: String, ctx: egui::Context) {
        let addr = adb::normalize_addr(&addr);
        if addr.is_empty() || self.connecting {
            return;
        }
        self.connecting = true;
        self.capture_msg = Some(format!("🔗 Connecting to {addr}…"));
        let slot = self.connect_result.clone();
        std::thread::spawn(move || {
            let res = adb::connect(&addr).map_err(|e| format!("{e:#}"));
            *slot.lock().unwrap() = Some((addr, res));
            ctx.request_repaint();
        });
    }

    /// Connect every remembered address (sequentially) on one background thread.
    fn start_connect_all(&mut self, ctx: egui::Context) {
        if self.connecting || self.remotes.is_empty() {
            return;
        }
        self.connecting = true;
        self.capture_msg = Some("🔗 Connecting saved devices…".into());
        let slot = self.connect_result.clone();
        let remotes = self.remotes.clone();
        std::thread::spawn(move || {
            let mut ok = 0usize;
            for r in &remotes {
                if adb::connect(r).is_ok() {
                    ok += 1;
                }
            }
            *slot.lock().unwrap() =
                Some(("*".to_string(), Ok(format!("connected {ok}/{}", remotes.len()))));
            ctx.request_repaint();
        });
    }

    /// Apply the result of a finished `adb connect`.
    fn poll_connect(&mut self) {
        let taken = self.connect_result.lock().unwrap().take();
        let Some((addr, res)) = taken else { return };
        self.connecting = false;
        match res {
            Ok(msg) => {
                self.capture_msg = Some(format!("🔗 {msg}"));
                if !self.remotes.contains(&addr) {
                    self.remotes.push(addr.clone());
                }
                self.refresh_devices();
                // Prefer the freshly-connected device.
                if self.devices.iter().any(|d| d.serial == addr && d.state == "device") {
                    self.selected_serial = Some(addr);
                    self.request_display_fetch();
                }
            }
            Err(e) => self.capture_msg = Some(format!("Connect failed: {e}")),
        }
    }

    /// Persist settings shortly after they stop changing (debounced).
    fn autosave(&mut self) {
        let cur = self.current_config();
        if cur != self.persisted {
            // Something changed; (re)start the debounce timer.
            if self.dirty_since.is_none() {
                self.dirty_since = Some(Instant::now());
            }
        }
        if let Some(t) = self.dirty_since {
            if t.elapsed().as_millis() >= 500 {
                let cur = self.current_config();
                cur.save();
                self.persisted = cur;
                self.dirty_since = None;
            }
        }
    }

    /// Save the currently-visible crop as a PNG next to the executable.
    fn take_screenshot(&mut self) {
        let Some(frame) = &self.last_frame else {
            self.capture_msg = Some("No frame to capture yet.".into());
            return;
        };
        match save_crop_png(frame, self.uv) {
            Ok(path) => self.capture_msg = Some(format!("📷 Saved {}", path.display())),
            Err(e) => self.capture_msg = Some(format!("Screenshot failed: {e}")),
        }
    }

    /// Toggle clip recording on the active stream.
    fn toggle_recording(&mut self) {
        let recording = self.stream.as_ref().map(|s| s.is_recording()).unwrap_or(false)
            || self.flat.as_ref().map(|f| f.is_recording()).unwrap_or(false);
        if self.stream.is_none() && self.flat.is_none() {
            return;
        }
        if recording {
            if let Some(s) = &self.stream {
                s.stop_recording();
            }
            if let Some(f) = &self.flat {
                f.stop_recording();
            }
            self.capture_msg = Some("⏹ Recording saved.".into());
        } else {
            match capture_path("clip", "mp4") {
                Ok(path) => {
                    if let Some(f) = &self.flat {
                        // The flat view is already what's on screen — record as-is.
                        f.start_recording(path.clone());
                    } else if let Some(s) = &self.stream {
                        // Capture exactly what's on screen: current crop + lens + tilt.
                        let view = ViewParams {
                            uv: [self.uv.min.x, self.uv.min.y, self.uv.max.x, self.uv.max.y],
                            lens_correct: self.lens_correct,
                            k1: self.lens_k1,
                            k2: self.lens_k2,
                            rotation_deg: self.rotation_deg,
                        };
                        s.start_recording(path.clone(), view);
                    }
                    self.capture_msg = Some(format!("⏺ Recording to {}", path.display()));
                }
                Err(e) => self.capture_msg = Some(format!("Record failed: {e}")),
            }
        }
    }

    /// Is the selected device a Meta/Quest headset? Those default to the flat
    /// view (the "meta protocol"); the panel crop/lens/tilt tools don't apply.
    fn selected_is_quest(&self) -> bool {
        self.selected_serial
            .as_ref()
            .and_then(|s| self.devices.iter().find(|d| &d.serial == s))
            .map(|d| {
                let m = d.model.to_lowercase();
                m.contains("quest") || m.contains("oculus") || m.contains("eureka") || m.contains("meta")
            })
            .unwrap_or(false)
    }

    fn active_config_matches(&self) -> bool {
        // The flat view has its own knobs (size/bitrate/fps/audio) — it needs a
        // reconnect to pick up changes just like the panel mirror does.
        if let Some(f) = &self.flat {
            let want = self.flat_config();
            return f.config == want;
        }
        let Some(s) = &self.stream else { return false };
        s.config.display_id == self.settings.display_id
            && s.config.max_size == self.settings.max_size
            && s.config.video_bit_rate == self.settings.bitrate_mbps * 1_000_000
            && s.config.max_fps == self.settings.max_fps
            && s.config.audio == self.settings.audio
    }

    /// The flat-view parameters implied by the current settings.
    fn flat_config(&self) -> crate::flat::FlatConfig {
        crate::flat::FlatConfig {
            max_size: if self.settings.max_size >= 640 { self.settings.max_size } else { 1920 },
            bitrate: self.settings.bitrate_mbps.max(2) * 1_000_000,
            fps: if self.settings.max_fps > 0 { self.settings.max_fps } else { 72 },
            audio: self.settings.audio,
        }
    }

    /// Pull the newest decoded frame into the GPU texture, if any.
    fn sync_texture(&mut self, ctx: &egui::Context) {
        let slot_arc = if let Some(f) = &self.flat {
            f.slot.clone()
        } else if let Some(s) = &self.stream {
            s.slot.clone()
        } else {
            return;
        };
        let mut slot = slot_arc.lock().unwrap();
        if slot.generation == self.last_generation {
            return;
        }
        if let Some(frame) = slot.frame.take() {
            self.last_generation = slot.generation;
            drop(slot);
            let size = [frame.width as usize, frame.height as usize];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
            self.tex_size = size;
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("mirror", image, egui::TextureOptions::LINEAR))
                }
            }
            // Keep the raw frame around for screenshots.
            self.last_frame = Some(frame);
        }
    }
}

impl eframe::App for App {
    // Non-painting work runs here, before `ui`.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.want_autostart && self.selected_serial.is_some() && self.stream.is_none() {
            self.want_autostart = false;
            self.request_display_fetch(); // happens once the stream is up
            self.connect(ctx);
        }
        self.poll_connect();
        self.poll_displays();
        self.drive_display_fetch();
        self.sync_texture(ctx);
        self.autosave();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.top_bar(ui);
        self.bottom_bar(ui);
        self.central_video(ui);
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("controls").show_inside(ui, |ui| {
            let ctx = ui.ctx().clone();
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                // Device picker.
                let dev_label = self
                    .selected_serial
                    .as_ref()
                    .and_then(|s| self.devices.iter().find(|d| &d.serial == s))
                    .map(|d| d.label())
                    .unwrap_or_else(|| "No device".into());
                let mut changed_device = false;
                egui::ComboBox::from_id_salt("device")
                    .selected_text(dev_label)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for d in self.devices.iter().filter(|d| d.state == "device") {
                            if ui
                                .selectable_label(
                                    self.selected_serial.as_deref() == Some(&d.serial),
                                    d.label(),
                                )
                                .clicked()
                            {
                                self.selected_serial = Some(d.serial.clone());
                                changed_device = true;
                            }
                        }
                    });
                if ui.button("⟳").on_hover_text("Refresh devices").clicked() {
                    self.refresh_devices();
                }
                if changed_device {
                    self.request_display_fetch();
                }

                ui.separator();

                // Display picker.
                let disp_label = self
                    .displays
                    .iter()
                    .find(|d| d.id == self.settings.display_id)
                    .map(|d| d.label())
                    .unwrap_or_else(|| format!("Display {}", self.settings.display_id));
                egui::ComboBox::from_id_salt("display")
                    .selected_text(disp_label)
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        if self.displays.is_empty() {
                            ui.label("(querying…)");
                        }
                        for d in &self.displays {
                            ui.selectable_value(&mut self.settings.display_id, d.id, d.label());
                        }
                    });
            });

            // Wireless adb: connect to / remember Quests over the network.
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Wi-Fi:").on_hover_text(
                    "adb connect a Quest over the network. Enable wireless debugging on \
                     the headset (or run `adb tcpip 5555` over USB first).",
                );
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.remote_input)
                        .hint_text("192.168.x.x[:5555]")
                        .desired_width(150.0),
                );
                let entered =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked = ui
                    .add_enabled(!self.connecting, egui::Button::new("🔗 Connect"))
                    .clicked();
                if (clicked || entered) && !self.remote_input.trim().is_empty() {
                    let addr = self.remote_input.trim().to_string();
                    self.remote_input.clear();
                    self.start_remote_connect(addr, ctx.clone());
                }
                if self.connecting {
                    ui.spinner();
                }

                if !self.remotes.is_empty() {
                    ui.separator();
                    if ui
                        .add_enabled(!self.connecting, egui::Button::new("Connect all"))
                        .on_hover_text("adb connect every saved address")
                        .clicked()
                    {
                        self.start_connect_all(ctx.clone());
                    }
                    let mut connect_one = None;
                    let mut forget = None;
                    for r in self.remotes.clone() {
                        if ui
                            .add_enabled(!self.connecting, egui::Button::new(format!("🔗 {r}")))
                            .on_hover_text("Reconnect & select")
                            .clicked()
                        {
                            connect_one = Some(r.clone());
                        }
                        if ui.small_button("✕").on_hover_text("Forget").clicked() {
                            forget = Some(r.clone());
                        }
                    }
                    if let Some(r) = connect_one {
                        self.start_remote_connect(r, ctx.clone());
                    }
                    if let Some(r) = forget {
                        adb::disconnect(&r);
                        self.remotes.retain(|x| x != &r);
                    }
                }
            });

            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                // Resolution cap.
                ui.label("Max size:");
                let size_label = if self.settings.max_size == 0 {
                    "Full panel".to_string()
                } else {
                    format!("{}px", self.settings.max_size)
                };
                egui::ComboBox::from_id_salt("maxsize")
                    .selected_text(size_label)
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for (val, txt) in [
                            (0u32, "Full panel"),
                            (1280, "1280px"),
                            (1600, "1600px"),
                            (1920, "1920px"),
                            (2560, "2560px"),
                            (3840, "3840px"),
                        ] {
                            ui.selectable_value(&mut self.settings.max_size, val, txt);
                        }
                    });

                ui.separator();
                ui.label("Bitrate:");
                ui.add(
                    egui::DragValue::new(&mut self.settings.bitrate_mbps)
                        .range(2..=80)
                        .suffix(" Mbps"),
                );

                ui.separator();
                ui.label("FPS:");
                let fps_label = if self.settings.max_fps == 0 {
                    "Max".to_string()
                } else {
                    self.settings.max_fps.to_string()
                };
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(fps_label)
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for (val, txt) in [
                            (0u32, "Max"),
                            (30, "30"),
                            (60, "60"),
                            (72, "72"),
                            (90, "90"),
                            (120, "120"),
                        ] {
                            ui.selectable_value(&mut self.settings.max_fps, val, txt);
                        }
                    });

                ui.separator();
                ui.checkbox(&mut self.settings.audio, "🔊 Audio");
            });

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let streaming = self.stream.is_some() || self.flat.is_some();
                if !streaming {
                    let enabled = self.selected_serial.is_some();
                    let quest = self.selected_is_quest();
                    if ui
                        .add_enabled(enabled, egui::Button::new("▶  Connect"))
                        .on_hover_text(if quest {
                            "Live flat view (default for Meta/Quest): whole undistorted view, unrooted"
                        } else {
                            "Mirror this device"
                        })
                        .clicked()
                    {
                        if quest {
                            self.connect_flat(&ctx);
                        } else {
                            self.connect(&ctx);
                        }
                    }
                    // Escape hatch for Quest: the raw panel mirror (stereo, lens-
                    // warped) with the crop/flatten tools.
                    if quest
                        && ui
                            .add_enabled(enabled, egui::Button::new("Panel view"))
                            .on_hover_text("Raw panel mirror (stereo, lens-warped) — enables the crop/flatten tools")
                            .clicked()
                    {
                        self.connect(&ctx);
                    }
                } else {
                    if ui.button("■  Disconnect").clicked() {
                        self.disconnect();
                    }
                    if (self.stream.is_some() || self.flat.is_some())
                        && !self.active_config_matches()
                        && ui
                            .button("⟳  Apply settings")
                            .on_hover_text("Reconnect with the new settings")
                            .clicked()
                    {
                        if self.flat.is_some() {
                            self.connect_flat(&ctx);
                        } else {
                            self.connect(&ctx);
                        }
                    }
                }

                ui.separator();
                self.status_label(ui);

                // Capture controls — work for either source (panel or flat view).
                if self.stream.is_some() || self.flat.is_some() {
                    ui.separator();
                    let can_shoot = self.texture.is_some();
                    if ui
                        .add_enabled(can_shoot, egui::Button::new("📷 Screenshot"))
                        .on_hover_text("Save the current crop as a PNG")
                        .clicked()
                    {
                        self.take_screenshot();
                    }
                    let recording = self.stream.as_ref().map(|s| s.is_recording()).unwrap_or(false)
                        || self.flat.as_ref().map(|f| f.is_recording()).unwrap_or(false);
                    let (label, color) = if recording {
                        ("⏹ Stop", Color32::from_rgb(0xff, 0x6b, 0x6b))
                    } else {
                        ("⏺ Record", Color32::from_rgb(0xff, 0xc4, 0x4c))
                    };
                    if ui
                        .add(egui::Button::new(egui::RichText::new(label).color(color)))
                        .on_hover_text("Record the full stream to an .mp4 clip")
                        .clicked()
                    {
                        self.toggle_recording();
                    }
                }
            });

            if let Some(msg) = &self.capture_msg {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(msg).weak().small());
            }
            ui.add_space(4.0);
        });
    }

    fn status_label(&self, ui: &mut egui::Ui) {
        let status = if let Some(f) = &self.flat {
            f.status()
        } else if let Some(s) = &self.stream {
            s.status()
        } else {
            ui.label("Idle");
            return;
        };
        match status {
            Status::Connecting => {
                ui.spinner();
                ui.label("Connecting…");
            }
            Status::Streaming { width, height, fps } => {
                ui.colored_label(
                    Color32::from_rgb(0x4c, 0xd9, 0x6a),
                    format!("● {width}×{height}  ·  {fps:.0} fps"),
                );
            }
            Status::Error(e) => {
                ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), format!("⚠ {e}"));
            }
            Status::Stopped => {
                ui.label("Stopped");
            }
        }
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("cropbar").show_inside(ui, |ui| {
            ui.add_space(3.0);
            // The crop / zoom / lens / tilt tools only apply to the raw (stereo,
            // lens-warped) panel mirror. In flat mode (the default for Meta/Quest)
            // the device already composites a flat, full, undistorted view — so
            // hide them. They re-appear for the Panel view or non-Meta devices.
            let show_tools =
                self.flat.is_none() && (self.stream.is_some() || !self.selected_is_quest());
            if !show_tools {
                ui.label(
                    egui::RichText::new(
                        "🥽 Flat view — whole undistorted stream (crop / lens / tilt not needed)",
                    )
                    .weak(),
                );
                ui.add_space(3.0);
                return;
            }
            ui.horizontal_wrapped(|ui| {
                // One-click "good Quest 3 view": left eye + lens flattened + level.
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("🥽 Quest 3").strong(),
                    ))
                    .on_hover_text("Apply the solid Quest 3 preset: left eye, lens flattened, no tilt")
                    .clicked()
                {
                    self.apply_quest3_preset();
                }
                ui.separator();
                ui.label("Crop:");
                if ui.button("Left eye").clicked() {
                    self.uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(0.5, 1.0));
                }
                if ui.button("Right eye").clicked() {
                    self.uv = Rect::from_min_max(pos2(0.5, 0.0), pos2(1.0, 1.0));
                }
                if ui.button("Full panel").clicked() {
                    self.uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                }
                if ui.button("Center 50%").clicked() {
                    self.uv = Rect::from_min_max(pos2(0.25, 0.25), pos2(0.75, 0.75));
                }
                ui.separator();
                // Zoom slider acts on the crop size around its centre.
                let mut zoom = 1.0 / self.uv.width().max(1e-3);
                if ui
                    .add(egui::Slider::new(&mut zoom, 1.0..=8.0).text("zoom"))
                    .changed()
                {
                    let center = self.uv.center();
                    let half = (0.5 / zoom).min(0.5);
                    let half_h = half; // keep square-ish in uv; aspect handled at draw
                    self.uv = clamp_uv(Rect::from_center_size(
                        center,
                        vec2(half * 2.0, half_h * 2.0),
                    ));
                }
                ui.label("· drag to pan, scroll to zoom");
            });

            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.lens_correct, "Flatten lens")
                    .on_hover_text("Undo the Quest's lens (fisheye) distortion for a flat 2D view");
                ui.add_enabled_ui(self.lens_correct, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.lens_k1, -0.8..=0.8)
                            .text("curve")
                            .fixed_decimals(2),
                    )
                    .on_hover_text("Main distortion: drag until straight lines look straight");
                    ui.add(
                        egui::Slider::new(&mut self.lens_k2, -0.4..=0.4)
                            .text("edge")
                            .fixed_decimals(2),
                    )
                    .on_hover_text("Fine correction near the edges");
                });
                ui.separator();
                ui.add(
                    egui::Slider::new(&mut self.rotation_deg, -45.0..=45.0)
                        .text("tilt°")
                        .fixed_decimals(1),
                )
                .on_hover_text("Rotate to straighten an inclined view");
                if ui.button("Reset").clicked() {
                    self.lens_k1 = QUEST3_K1;
                    self.lens_k2 = QUEST3_K2;
                    self.rotation_deg = QUEST3_TILT;
                }
            });
            ui.add_space(3.0);
        });
    }

    fn central_video(&mut self, ui: &mut egui::Ui) {
        let Some(texture) = self.texture.clone() else {
            ui.centered_and_justified(|ui| {
                let msg = if self.stream.is_some() || self.flat.is_some() {
                    "Waiting for first frame…"
                } else {
                    "Pick your Quest and hit Connect."
                };
                ui.label(egui::RichText::new(msg).size(16.0).weak());
            });
            return;
        };

            let area = ui.available_rect_before_wrap();
            let response = ui.allocate_rect(area, Sense::click_and_drag());
            let painter = ui.painter_at(area);

            let tex_w = self.tex_size[0] as f32;
            let tex_h = self.tex_size[1] as f32;
            let crop_w_px = self.uv.width() * tex_w;
            let crop_h_px = self.uv.height() * tex_h;
            let crop_aspect = (crop_w_px / crop_h_px).max(1e-3);
            let area_aspect = area.width() / area.height().max(1.0);

            let draw_size = if area_aspect > crop_aspect {
                vec2(area.height() * crop_aspect, area.height())
            } else {
                vec2(area.width(), area.width() / crop_aspect)
            };
            let draw_rect = Rect::from_center_size(area.center(), draw_size);

            painter.rect_filled(area, 0.0, Color32::from_gray(12));
            if self.lens_correct || self.rotation_deg.abs() > 0.01 {
                let (k1, k2) = if self.lens_correct {
                    (self.lens_k1, self.lens_k2)
                } else {
                    (0.0, 0.0)
                };
                draw_warped(&painter, texture.id(), draw_rect, self.uv, k1, k2, self.rotation_deg);
            } else {
                painter.image(texture.id(), draw_rect, self.uv, Color32::WHITE);
            }

            // Drag to pan.
            if response.dragged() {
                let d = response.drag_delta();
                let uv_dx = -d.x / draw_size.x * self.uv.width();
                let uv_dy = -d.y / draw_size.y * self.uv.height();
                self.uv = clamp_uv(self.uv.translate(vec2(uv_dx, uv_dy)));
            }

            // Scroll to zoom around the cursor.
            if response.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll.abs() > 0.1 {
                    if let Some(pos) = response.hover_pos() {
                        let factor = (1.0 - scroll * 0.0015).clamp(0.5, 1.5);
                        let cursor_uv = screen_to_uv(pos, draw_rect, self.uv);
                        self.uv = clamp_uv(scale_about(self.uv, cursor_uv, factor));
                    }
                }
            }
    }
}

/// Add a monochrome emoji font (bundled Noto Emoji) as a fallback so the UI's
/// 🥽🔊📷⏺🔗 glyphs render — egui's built-in font only covers a tiny icon
/// subset, and color fonts (Segoe UI Emoji) don't render in egui's rasterizer.
fn install_emoji_font(ctx: &egui::Context) {
    const NOTO_EMOJI: &[u8] = include_bytes!("../assets/NotoEmoji-Regular.ttf");
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("noto_emoji".to_owned(), Arc::new(egui::FontData::from_static(NOTO_EMOJI)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("noto_emoji".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// `captures/` folder next to the executable (created on demand).
fn capture_dir() -> std::io::Result<PathBuf> {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("captures");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A timestamped path like `captures/clip_1718000000.mp4`.
fn capture_path(prefix: &str, ext: &str) -> std::io::Result<PathBuf> {
    let dir = capture_dir()?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(dir.join(format!("{prefix}_{ts}.{ext}")))
}

/// Crop the current frame to the visible `uv` window and save it as PNG.
fn save_crop_png(frame: &Frame, uv: Rect) -> anyhow::Result<PathBuf> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 {
        anyhow::bail!("empty frame");
    }
    let wi = w as f32;
    let hi = h as f32;
    let x0 = (uv.min.x * wi).floor().clamp(0.0, wi - 1.0) as usize;
    let y0 = (uv.min.y * hi).floor().clamp(0.0, hi - 1.0) as usize;
    let x1 = ((uv.max.x * wi).ceil() as usize).clamp(x0 + 1, w);
    let y1 = ((uv.max.y * hi).ceil() as usize).clamp(y0 + 1, h);
    let cw = x1 - x0;
    let ch = y1 - y0;

    let mut out = vec![0u8; cw * ch * 4];
    for row in 0..ch {
        let src = ((y0 + row) * w + x0) * 4;
        let dst = row * cw * 4;
        out[dst..dst + cw * 4].copy_from_slice(&frame.rgba[src..src + cw * 4]);
    }

    let path = capture_path("quest", "png")?;
    let img = image::RgbaImage::from_raw(cw as u32, ch as u32, out)
        .ok_or_else(|| anyhow::anyhow!("frame buffer size mismatch"))?;
    img.save(&path)?;
    Ok(path)
}

/// Draw the cropped texture through a tessellated mesh that (a) undoes the
/// Quest's radial lens distortion and (b) applies an in-plane rotation, turning
/// the fisheye VR view into a flat, upright 2D image.
///
/// For each grid point we treat `(ox, oy) ∈ [-1,1]²` as the *undistorted* output
/// position; the matching source sample sits at radius scaled by
/// `1 + k1·r² + k2·r⁴`, and the on-screen position is that point rotated rigidly
/// about the centre.
fn draw_warped(
    painter: &egui::Painter,
    tex: egui::TextureId,
    rect: Rect,
    uv: Rect,
    k1: f32,
    k2: f32,
    rot_deg: f32,
) {
    use egui::epaint::{Mesh, Vertex};
    const N: usize = 48; // grid cells per axis

    let (sin, cos) = rot_deg.to_radians().sin_cos();
    let center = rect.center();
    let half_w = rect.width() * 0.5;
    let half_h = rect.height() * 0.5;

    let mut mesh = Mesh::with_texture(tex);
    for j in 0..=N {
        for i in 0..=N {
            let fx = i as f32 / N as f32;
            let fy = j as f32 / N as f32;
            let ox = fx * 2.0 - 1.0;
            let oy = fy * 2.0 - 1.0;

            // Radial remap: where to sample the distorted source for this output.
            let r2 = ox * ox + oy * oy;
            let f = 1.0 + k1 * r2 + k2 * r2 * r2;
            let sx = (0.5 + 0.5 * ox * f).clamp(0.0, 1.0);
            let sy = (0.5 + 0.5 * oy * f).clamp(0.0, 1.0);
            let u = uv.min.x + sx * uv.width();
            let v = uv.min.y + sy * uv.height();

            // Rigid rotation in pixel space about the centre.
            let px = ox * half_w;
            let py = oy * half_h;
            let pos = pos2(center.x + px * cos - py * sin, center.y + px * sin + py * cos);

            mesh.vertices.push(Vertex { pos, uv: pos2(u, v), color: Color32::WHITE });
        }
    }

    let row = (N + 1) as u32;
    for j in 0..N as u32 {
        for i in 0..N as u32 {
            let a = j * row + i;
            let b = a + 1;
            let c = a + row;
            let d = c + 1;
            mesh.indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Map a screen position inside `draw_rect` to a uv coordinate within `uv`.
fn screen_to_uv(pos: Pos2, draw_rect: Rect, uv: Rect) -> Pos2 {
    let fx = ((pos.x - draw_rect.min.x) / draw_rect.width()).clamp(0.0, 1.0);
    let fy = ((pos.y - draw_rect.min.y) / draw_rect.height()).clamp(0.0, 1.0);
    pos2(uv.min.x + fx * uv.width(), uv.min.y + fy * uv.height())
}

/// Scale `uv` about a fixed `anchor` (in uv space) by `factor`.
fn scale_about(uv: Rect, anchor: Pos2, factor: f32) -> Rect {
    let min = anchor + (uv.min - anchor) * factor;
    let max = anchor + (uv.max - anchor) * factor;
    Rect::from_min_max(min, max)
}

/// Keep a crop rect within [0,1]², clamping size to <= 1 and nudging it in-bounds.
fn clamp_uv(mut r: Rect) -> Rect {
    let mut w = r.width().clamp(0.02, 1.0);
    let mut h = r.height().clamp(0.02, 1.0);
    // Re-center if the requested size overflows.
    let cx = r.center().x;
    let cy = r.center().y;
    if w > 1.0 {
        w = 1.0;
    }
    if h > 1.0 {
        h = 1.0;
    }
    let mut min = pos2(cx - w / 2.0, cy - h / 2.0);
    if min.x < 0.0 {
        min.x = 0.0;
    }
    if min.y < 0.0 {
        min.y = 0.0;
    }
    if min.x + w > 1.0 {
        min.x = 1.0 - w;
    }
    if min.y + h > 1.0 {
        min.y = 1.0 - h;
    }
    r = Rect::from_min_size(min, vec2(w, h));
    r
}
