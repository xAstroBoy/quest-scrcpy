//! Clip recording via Media Foundation (Windows fallback when ffmpeg is absent).
//!
//! - [`Recorder`] muxes the incoming H.264 elementary stream straight into an
//!   `.mp4` (no re-encode, lossless, full panel). Used by the headless CLI.
//! - [`ClipEncoder`] takes already-processed BGRA frames (the cropped, lens-
//!   flattened view) and H.264-encodes them, so the GUI's "Record" matches what
//!   you actually see on screen.

use anyhow::{Result, anyhow, bail};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::PCWSTR;

const MF_VERSION_VAL: u32 = 0x0002_0070;

fn sink_writer(path: &Path) -> Result<IMFSinkWriter> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION_VAL, MFSTARTUP_NOSOCKET);

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned null"))?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        MFCreateSinkWriterFromURL(PCWSTR(wide.as_ptr()), None, &attrs)
            .map_err(|e| anyhow!("create MP4 sink writer: {e}"))
    }
}

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

/// Re-encodes already-processed BGRA frames (the cropped, lens-flattened view)
/// into an `.mp4`. Input is `MFVideoFormat_RGB32` (B,G,R,X bytes, top-down); the
/// sink writer inserts the colour-convert + H.264 encoder automatically.
pub struct ClipEncoder {
    writer: IMFSinkWriter,
    stream_index: u32,
    width: u32,
    height: u32,
    fps: i64,
    base_pts: Option<i64>,
    prev_time: i64,
    frames: u64,
}

impl ClipEncoder {
    pub fn new(path: &Path, width: u32, height: u32, fps_hint: u32, bitrate: u32) -> Result<Self> {
        let width = width.max(2) & !1; // H.264 needs even dimensions
        let height = height.max(2) & !1;
        let fps = if fps_hint > 0 { fps_hint as i64 } else { 60 };
        let bitrate = if bitrate > 0 { bitrate } else { 12_000_000 };

        unsafe {
            let writer = sink_writer(path)?;

            // Target: H.264.
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            let stream_index = writer.AddStream(&out).map_err(|e| anyhow!("AddStream: {e}"))?;

            // Source: RGB32 (BGRX), top-down (positive default stride).
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
            writer
                .SetInputMediaType(stream_index, &inp, None)
                .map_err(|e| anyhow!("SetInputMediaType(RGB32): {e}"))?;

            writer.BeginWriting().map_err(|e| anyhow!("BeginWriting: {e}"))?;
            Ok(Self {
                writer,
                stream_index,
                width,
                height,
                fps,
                base_pts: None,
                prev_time: 0,
                frames: 0,
            })
        }
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// `bgra` must be `width*height*4` bytes (B,G,R,X), top-down.
    pub fn write_bgra(&mut self, bgra: &[u8], pts_us: u64) -> Result<()> {
        let need = (self.width * self.height * 4) as usize;
        if bgra.len() < need {
            bail!("frame buffer too small: {} < {}", bgra.len(), need);
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(need as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), ptr, need);
            buffer.Unlock()?;
            buffer.SetCurrentLength(need as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;

            let pts100 = pts_us as i64 * 10;
            let base = *self.base_pts.get_or_insert(pts100);
            let mut t = (pts100 - base).max(0);
            let nominal = (10_000_000 / self.fps).max(1);
            if self.frames > 0 && t <= self.prev_time {
                t = self.prev_time + nominal;
            }
            sample.SetSampleTime(t)?;
            sample.SetSampleDuration(nominal)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_valid_mp4() {
        // Encode a few synthetic gradient frames and confirm we get a real MP4.
        let path = std::env::temp_dir().join("quest_scrcpy_enc_test.mp4");
        let _ = std::fs::remove_file(&path);
        let (w, h) = (320u32, 240u32);
        let mut enc = ClipEncoder::new(&path, w, h, 30, 4_000_000).expect("create encoder");
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for f in 0..30u32 {
            for y in 0..h {
                for x in 0..w {
                    let o = ((y * w + x) * 4) as usize;
                    buf[o] = (x + f * 4) as u8; // B
                    buf[o + 1] = (y + f * 4) as u8; // G
                    buf[o + 2] = (f * 8) as u8; // R
                    buf[o + 3] = 255;
                }
            }
            enc.write_bgra(&buf, (f as u64) * 33_333).expect("write frame");
        }
        enc.finalize().expect("finalize");

        let bytes = std::fs::read(&path).expect("read mp4");
        assert!(bytes.len() > 1000, "mp4 too small: {}", bytes.len());
        assert_eq!(&bytes[4..8], b"ftyp", "not an mp4 container");
        let _ = std::fs::remove_file(&path);
    }
}
