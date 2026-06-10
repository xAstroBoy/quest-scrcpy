//! Clip recorder: muxes the incoming H.264 elementary stream straight into an
//! `.mp4` using the built-in Media Foundation sink writer. No re-encode, no
//! ffmpeg — the encoded access units scrcpy already sends are written verbatim,
//! so recording is essentially free and lossless.

use anyhow::{Result, anyhow};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::PCWSTR;

const MF_VERSION_VAL: u32 = 0x0002_0070;

pub struct Recorder {
    writer: IMFSinkWriter,
    stream_index: u32,
    fps: i64,
    base_pts: Option<i64>,
    prev_time: i64,
    frames: u64,
}

impl Recorder {
    /// `config` is the Annex-B SPS/PPS from the stream's config packet.
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, config: &[u8]) -> Result<Self> {
        let fps = if fps_hint > 0 { fps_hint as i64 } else { 60 };
        unsafe {
            // Be self-contained: works whether or not the caller already set up
            // COM/MF (the GUI decode thread has; a headless CLI run has not).
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION_VAL, MFSTARTUP_NOSOCKET);

            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

            // Don't throttle: we feed faster than realtime when recording history.
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned null"))?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

            let writer = MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)
                .map_err(|e| anyhow!("create MP4 sink writer: {e}"))?;

            // Target (MP4-stored) type and source type are both H.264 → passthrough.
            let out = make_h264_type(width, height, fps, config)?;
            let stream_index = writer
                .AddStream(&out)
                .map_err(|e| anyhow!("AddStream: {e}"))?;

            let inp = make_h264_type(width, height, fps, config)?;
            writer
                .SetInputMediaType(stream_index, &inp, None)
                .map_err(|e| anyhow!("SetInputMediaType: {e}"))?;

            writer.BeginWriting().map_err(|e| anyhow!("BeginWriting: {e}"))?;

            Ok(Self {
                writer,
                stream_index,
                fps,
                base_pts: None,
                prev_time: 0,
                frames: 0,
            })
        }
    }

    /// Write one access unit. `pts_us` is the scrcpy timestamp in microseconds.
    pub fn write(&mut self, au: &[u8], pts_us: u64, key: bool) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(au.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;

            // Rebase to zero and convert microseconds -> 100ns units.
            let pts100 = pts_us as i64 * 10;
            let base = *self.base_pts.get_or_insert(pts100);
            let mut t = (pts100 - base).max(0);
            let nominal = (10_000_000 / self.fps).max(1);
            if self.frames > 0 && t <= self.prev_time {
                t = self.prev_time + nominal; // keep timestamps strictly increasing
            }
            sample.SetSampleTime(t)?;
            sample.SetSampleDuration(nominal)?;
            if key {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }

            self.prev_time = t;
            self.frames += 1;
            self.writer
                .WriteSample(self.stream_index, &sample)
                .map_err(|e| anyhow!("WriteSample: {e}"))?;
        }
        Ok(())
    }

    pub fn finalize(self) -> Result<()> {
        unsafe {
            self.writer.Finalize().map_err(|e| anyhow!("Finalize: {e}"))?;
        }
        Ok(())
    }
}

/// Build an H.264 media type carrying the SPS/PPS as the sequence header so the
/// MP4 muxer can construct the `avcC` box from our Annex-B stream.
unsafe fn make_h264_type(width: u32, height: u32, fps: i64, config: &[u8]) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, 16_000_000)?;
        if !config.is_empty() {
            t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, config)?;
        }
        Ok(t)
    }
}
