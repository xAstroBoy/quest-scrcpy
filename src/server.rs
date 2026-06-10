//! scrcpy-server v4.0 wire protocol (host/client side).
//!
//! Forward-tunnel flow: the server listens on `localabstract:scrcpy`; we connect
//! out to the adb-forwarded local port. The server accepts sockets in a fixed
//! order — video, then audio, then control — and **all** accepts must complete
//! before it sends the device meta. So when audio is enabled we must connect
//! both sockets up front, then read the metadata; reading the device name on the
//! video socket before connecting the audio socket would deadlock.
//!
//! Stream framing (with `send_*_meta=true`):
//!   first socket only:  [dummy byte 0x00][device name, 64 bytes]
//!   video socket:       [codec id u32]
//!   audio socket:       [codec id u32]      (or a 4-byte disable code: 0/1)
//!   then per packet:    [pts/flags u64][size u32][size bytes]
//!
//! v4.0 framing change vs v2.0: the video resolution is no longer part of the
//! codec header. Instead it arrives in-band as a "session meta" packet whose
//! header has the SESSION flag set, and the media flag bits shifted down one:
//!   SESSION   = 1<<63   header = [flags u32][width u32][height u32]
//!   CONFIG    = 1<<62
//!   KEY_FRAME = 1<<61

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

pub const PACKET_FLAG_SESSION: u64 = 1 << 63;
pub const PACKET_FLAG_CONFIG: u64 = 1 << 62;
pub const PACKET_FLAG_KEY_FRAME: u64 = 1 << 61;
/// PTS occupies the low 61 bits (everything below the flag bits).
const PTS_MASK: u64 = PACKET_FLAG_KEY_FRAME - 1;

const DEVICE_NAME_LEN: usize = 64;

pub const CODEC_ID_H264: u32 = u32::from_be_bytes(*b"h264");
pub const CODEC_ID_AAC: u32 = u32::from_be_bytes([0, b'a', b'a', b'c']);
pub const CODEC_ID_OPUS: u32 = u32::from_be_bytes(*b"opus");

/// Device disabled audio capture but wants video to continue.
pub const AUDIO_DISABLED: u32 = 0;
/// Device hit a fatal audio configuration error.
pub const AUDIO_CONFIG_ERROR: u32 = 1;

#[derive(Clone, Debug)]
pub struct VideoMeta {
    pub device_name: String,
    pub codec_id: u32,
}

#[derive(Clone, Debug)]
pub struct AudioMeta {
    pub codec_id: u32,
}

/// A fully-established mirror connection: video socket + meta, plus the audio
/// socket if audio was requested (and the device didn't refuse it).
pub struct Connection {
    pub video: TcpStream,
    pub video_meta: VideoMeta,
    pub audio: Option<(TcpStream, AudioMeta)>,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub is_config: bool,
    pub is_key: bool,
    pub pts: u64,
    pub data: Vec<u8>,
}

/// One thing read off a media socket: either a resolution announcement (video
/// only) or an actual media packet.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Resolution { width: u32, height: u32 },
    Packet(Packet),
}

fn read_exact_or_eof(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    stream
        .read_exact(buf)
        .context("connection closed mid-stream")?;
    Ok(())
}

fn read_u32(stream: &mut TcpStream) -> Result<u32> {
    let mut b = [0u8; 4];
    read_exact_or_eof(stream, &mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(stream: &mut TcpStream) -> Result<u64> {
    let mut b = [0u8; 8];
    read_exact_or_eof(stream, &mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Connect to the forwarded port, retrying until the server's socket is live.
/// adb forward accepts immediately even before the device server is up, so a
/// successful connect doesn't yet mean the server is listening.
fn connect_retry(port: u16, deadline: Instant) -> Result<TcpStream> {
    let addr = format!("127.0.0.1:{port}");
    let mut last_err = None;
    while Instant::now() < deadline {
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    match last_err {
        Some(e) => Err(e).context("could not connect to forwarded scrcpy port"),
        None => bail!("timed out connecting to scrcpy port"),
    }
}

/// Establish the full connection. Connects the video socket (and the audio
/// socket, if requested) *before* reading any metadata, because the server only
/// sends the device meta once every requested socket has been accepted.
pub fn connect(port: u16, want_audio: bool, timeout: Duration) -> Result<Connection> {
    let deadline = Instant::now() + timeout;

    // 1. Video socket: connect and read the dummy byte. Retry the whole step
    //    while the device-side server is still starting up.
    let mut video = loop {
        let mut stream = connect_retry(port, deadline)?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut dummy = [0u8; 1];
        if stream.read_exact(&mut dummy).is_ok() {
            break stream;
        }
        stream.shutdown(Shutdown::Both).ok();
        if Instant::now() >= deadline {
            bail!("scrcpy server never delivered the handshake byte");
        }
        std::thread::sleep(Duration::from_millis(120));
    };

    // 2. Audio socket (no dummy byte — that only goes to the first socket).
    //    Connecting it now lets the server's accept() loop finish so it will
    //    proceed to send the device meta on the video socket below.
    let mut audio_sock = if want_audio {
        let s = connect_retry(port, deadline)?;
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        Some(s)
    } else {
        None
    };

    // 3. Device name + video codec id (video socket).
    let mut name = [0u8; DEVICE_NAME_LEN];
    read_exact_or_eof(&mut video, &mut name)?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    let device_name = String::from_utf8_lossy(&name[..end]).into_owned();
    let video_codec = read_u32(&mut video)?;
    video.set_read_timeout(None).ok();

    // 4. Audio codec id (audio socket), if any.
    let audio = match audio_sock.take() {
        Some(mut s) => {
            let codec = read_u32(&mut s)?;
            s.set_read_timeout(None).ok();
            Some((s, AudioMeta { codec_id: codec }))
        }
        None => None,
    };

    Ok(Connection {
        video,
        video_meta: VideoMeta { device_name, codec_id: video_codec },
        audio,
    })
}

/// Read one event (frame-meta header + optional payload) off a media socket.
pub fn read_event(stream: &mut TcpStream) -> Result<StreamEvent> {
    let header = read_u64(stream)?;

    // Session meta: the header's low 32 bits are the width, then a u32 height.
    if header & PACKET_FLAG_SESSION != 0 {
        let width = (header & 0xFFFF_FFFF) as u32;
        let height = read_u32(stream)?;
        return Ok(StreamEvent::Resolution { width, height });
    }

    let size = read_u32(stream)? as usize;
    if size == 0 || size > 64 * 1024 * 1024 {
        bail!("implausible packet size {size}");
    }
    let mut data = vec![0u8; size];
    read_exact_or_eof(stream, &mut data)?;
    Ok(StreamEvent::Packet(Packet {
        is_config: header & PACKET_FLAG_CONFIG != 0,
        is_key: header & PACKET_FLAG_KEY_FRAME != 0,
        pts: header & PTS_MASK,
        data,
    }))
}

pub fn codec_name(id: u32) -> String {
    match id {
        CODEC_ID_H264 => "h264".into(),
        CODEC_ID_AAC => "aac".into(),
        CODEC_ID_OPUS => "opus".into(),
        AUDIO_DISABLED => "disabled".into(),
        AUDIO_CONFIG_ERROR => "config-error".into(),
        other => format!("0x{other:08x}"),
    }
}
