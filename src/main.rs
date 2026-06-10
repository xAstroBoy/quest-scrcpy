// Hide the console window for the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod app;
mod audio;
mod audioplay;
mod config;
mod decoder;
mod recorder;
mod server;
mod stream;

use anyhow::{Result, anyhow};
use app::StartupConfig;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "quest-scrcpy",
    version,
    about = "Native scrcpy client tuned for the Meta Quest (one-eye live crop)."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the GUI (default when no subcommand is given).
    Gui(MirrorArgs),
    /// Launch the GUI preconfigured (and auto-connect) with these options.
    Mirror(MirrorArgs),
    /// List connected adb devices.
    List,
    /// List the displays of a device.
    Displays {
        /// Device serial (defaults to the first/USB device).
        #[arg(short, long)]
        serial: Option<String>,
    },
    /// Grab a single frame to PNG headlessly, then exit (diagnostic).
    Shot {
        #[command(flatten)]
        args: MirrorArgs,
        /// Output .png path.
        #[arg(short, long, default_value = "shot.png")]
        output: String,
    },
    /// Record a clip to .mp4 headlessly (no window), then exit.
    Record {
        #[command(flatten)]
        args: MirrorArgs,
        /// Seconds to record.
        #[arg(short = 'n', long, default_value_t = 10)]
        seconds: u64,
        /// Output .mp4 path.
        #[arg(short, long, default_value = "clip.mp4")]
        output: String,
    },
}

#[derive(Parser, Default)]
struct MirrorArgs {
    /// Device serial (defaults to the first/USB device).
    #[arg(short, long)]
    serial: Option<String>,
    /// Display id to mirror.
    #[arg(short, long)]
    display: Option<u32>,
    /// Cap the longest side, in pixels (0 = full panel).
    #[arg(short = 'm', long)]
    max_size: Option<u32>,
    /// Video bitrate in Mbps.
    #[arg(short, long)]
    bitrate: Option<u32>,
    /// Frame-rate cap (0 = unlimited).
    #[arg(short, long)]
    fps: Option<u32>,
    /// Enable audio forwarding.
    #[arg(short, long)]
    audio: bool,
}

fn pick_serial(explicit: Option<String>) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s);
    }
    let devices = adb::list_devices()?;
    let online: Vec<_> = devices.into_iter().filter(|d| d.state == "device").collect();
    online
        .iter()
        .find(|d| d.usb)
        .or_else(|| online.first())
        .map(|d| d.serial.clone())
        .ok_or_else(|| anyhow!("no online adb devices"))
}

fn run_gui(args: MirrorArgs, autostart: bool) -> Result<()> {
    let startup = StartupConfig {
        serial: args.serial,
        display_id: args.display,
        max_size: args.max_size,
        bitrate_mbps: args.bitrate,
        max_fps: args.fps,
        audio: if args.audio { Some(true) } else { None },
        autostart,
    };

    let mut options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Quest scrcpy"),
        ..Default::default()
    };
    // Don't vsync-queue presents — show the freshest decoded frame immediately
    // (vsync would add up to a refresh interval of latency).
    options.wgpu_options.present_mode = eframe::wgpu::PresentMode::AutoNoVsync;

    eframe::run_native(
        "Quest scrcpy",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, startup)))),
    )
    .map_err(|e| anyhow!("failed to start GUI: {e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_gui(MirrorArgs::default(), false),
        Some(Command::Gui(args)) => run_gui(args, false),
        Some(Command::Mirror(args)) => run_gui(args, true),
        Some(Command::List) => {
            let devices = adb::list_devices()?;
            if devices.is_empty() {
                println!("No devices. Is the Quest connected and adb authorised?");
            }
            for d in devices {
                println!("{}  [{}]", d.label(), d.state);
            }
            Ok(())
        }
        Some(Command::Displays { serial }) => {
            let serial = pick_serial(serial)?;
            println!("Displays on {serial}:");
            let displays = adb::list_displays(&serial)?;
            for d in displays {
                println!("  {}", d.label());
            }
            Ok(())
        }
        Some(Command::Record { args, seconds, output }) => {
            run_record(args, seconds, output.into())
        }
        Some(Command::Shot { args, output }) => run_shot(args, output.into()),
    }
}

/// Headless single-frame grab for diagnostics.
fn run_shot(args: MirrorArgs, output: std::path::PathBuf) -> Result<()> {
    use crate::adb::ServerOptions;
    use crate::decoder::H264Decoder;
    use std::time::{Duration, Instant};

    let serial = pick_serial(args.serial)?;
    adb::push_server(&serial)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .ok_or_else(|| anyhow!("could not allocate a local port"))?;
    adb::forward(&serial, port)?;

    let opts = ServerOptions {
        display_id: args.display.unwrap_or(0),
        max_size: args.max_size.unwrap_or(0),
        video_bit_rate: args.bitrate.unwrap_or(16) * 1_000_000,
        max_fps: args.fps.unwrap_or(0),
        audio: false,
        audio_bit_rate: 128_000,
    };
    let mut child = adb::start_server(&serial, &opts)?;

    let result = (|| -> Result<()> {
        let conn = server::connect(port, false, Duration::from_secs(12))?;
        let mut video = conn.video;
        let mut decoder: Option<H264Decoder> = None;
        let (mut sw, mut sh) = (0u32, 0u32);
        let deadline = Instant::now() + Duration::from_secs(8);

        while Instant::now() < deadline {
            match server::read_event(&mut video)? {
                server::StreamEvent::Resolution { width, height } => {
                    sw = width;
                    sh = height;
                    decoder = Some(H264Decoder::new(width.max(1), height.max(1))?);
                }
                server::StreamEvent::Packet(pkt) => {
                    if decoder.is_none() {
                        decoder = Some(H264Decoder::new(sw.max(1920), sh.max(1088))?);
                    }
                    let frames = decoder.as_mut().unwrap().decode(&pkt.data)?;
                    if let Some(frame) = frames.into_iter().last() {
                        let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
                            .ok_or_else(|| anyhow!("frame buffer mismatch"))?;
                        img.save(&output)?;
                        println!("Saved {} ({}x{})", output.display(), frame.width, frame.height);
                        return Ok(());
                    }
                }
            }
        }
        anyhow::bail!("no frame decoded within timeout")
    })();

    let _ = child.kill();
    adb::remove_forward(&serial, port);
    result
}

/// Headless capture: connect, mux the raw H.264 to MP4 for `seconds`, exit.
#[allow(unused_assignments)]
fn run_record(args: MirrorArgs, seconds: u64, output: std::path::PathBuf) -> Result<()> {
    use crate::adb::ServerOptions;
    use std::time::{Duration, Instant};

    let serial = pick_serial(args.serial)?;
    adb::push_server(&serial)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .ok_or_else(|| anyhow!("could not allocate a local port"))?;
    adb::forward(&serial, port)?;

    let max_fps = args.fps.unwrap_or(0);
    let opts = ServerOptions {
        display_id: args.display.unwrap_or(0),
        max_size: args.max_size.unwrap_or(0),
        video_bit_rate: args.bitrate.unwrap_or(16) * 1_000_000,
        max_fps,
        audio: false,
        audio_bit_rate: 128_000,
    };
    let mut child = adb::start_server(&serial, &opts)?;

    let result = (|| -> Result<()> {
        let conn = server::connect(port, false, Duration::from_secs(12))?;
        let mut video = conn.video;
        println!("Recording {seconds}s from {serial} -> {}…", output.display());

        let mut recorder: Option<recorder::Recorder> = None;
        let mut last_config: Vec<u8> = Vec::new();
        let mut waiting_key = true;
        let (mut sw, mut sh) = (0u32, 0u32);
        let deadline = Instant::now() + Duration::from_secs(seconds);

        while Instant::now() < deadline {
            match server::read_event(&mut video)? {
                server::StreamEvent::Resolution { width, height } => {
                    sw = width;
                    sh = height;
                }
                server::StreamEvent::Packet(pkt) => {
                    if pkt.is_config {
                        last_config = pkt.data.clone();
                        if recorder.is_none() && sw > 0 {
                            recorder = Some(recorder::Recorder::new(
                                &output, sw, sh, max_fps, &last_config,
                            )?);
                        }
                    }
                    if let Some(r) = recorder.as_mut() {
                        if !pkt.is_config {
                            if waiting_key && !pkt.is_key {
                                // wait for first keyframe
                            } else {
                                waiting_key = false;
                                r.write(&pkt.data, pkt.pts, pkt.is_key)?;
                            }
                        }
                    }
                }
            }
        }

        match recorder {
            Some(r) => {
                r.finalize()?;
                println!("Saved {}", output.display());
            }
            None => println!("No video received; nothing recorded."),
        }
        Ok(())
    })();

    let _ = child.kill();
    adb::remove_forward(&serial, port);
    result
}
