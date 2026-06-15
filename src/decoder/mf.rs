//! Hardware/­software H.264 decode via the built-in Windows Media Foundation
//! "Microsoft H264 Video Decoder" MFT. We feed Annex-B access units and get
//! NV12 frames back, which we convert to RGBA for the GUI.
//!
//! Zero external dependencies: this MFT ships with Windows. Used as the fallback
//! when ffmpeg is not on PATH (see the parent module).

use anyhow::{Result, anyhow, bail};
use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::GUID;

use super::Frame;

/// MF version dword for Vista+ (MF_SDK_VERSION<<16 | MF_API_VERSION).
const MF_VERSION_VAL: u32 = 0x0002_0070;
/// CLSID_CMSH264DecoderMFT — the Microsoft H.264 Video Decoder.
const CLSID_H264_DECODER: GUID = GUID::from_u128(0x62CE7E72_4C71_4D20_B15D_452831A87D9D);

pub struct MfDecoder {
    transform: IMFTransform,
    /// Caller-allocated output sample (the MS decoder does not provide its own).
    out_sample: Option<IMFSample>,
    /// Coded (padded) dimensions reported by the negotiated output type.
    coded_w: u32,
    coded_h: u32,
    stride: usize,
    /// Visible dimensions we actually emit (clamped to what scrcpy told us).
    vis_w: u32,
    vis_h: u32,
    /// Expected size from the scrcpy codec meta; clamps away macroblock padding.
    expect_w: u32,
    expect_h: u32,
    pts: i64,
}

impl MfDecoder {
    pub fn new(expect_w: u32, expect_h: u32) -> Result<Self> {
        unsafe {
            // MTA is fine for a worker thread; ignore "already initialized".
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION_VAL, MFSTARTUP_NOSOCKET)
                .map_err(|e| anyhow!("MFStartup failed: {e}"))?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_H264_DECODER, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| anyhow!("could not create H264 decoder MFT: {e}"))?;

            // Low-latency mode: tell the decoder not to buffer frames for
            // reordering (scrcpy emits no B-frames), so each access unit comes
            // straight back out instead of being held in the DPB.
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            // Input type: H.264, progressive, with a size hint.
            let input = MFCreateMediaType().map_err(|e| anyhow!("MFCreateMediaType: {e}"))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            let fs = ((expect_w as u64) << 32) | (expect_h as u64);
            input.SetUINT64(&MF_MT_FRAME_SIZE, fs)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| anyhow!("SetInputType(H264): {e}"))?;

            let mut dec = Self {
                transform,
                out_sample: None,
                coded_w: 0,
                coded_h: 0,
                stride: 0,
                vis_w: 0,
                vis_h: 0,
                expect_w,
                expect_h,
                pts: 0,
            };
            // The MS H.264 decoder requires an output type before ProcessOutput;
            // it derives a size from the input MF_MT_FRAME_SIZE we set above.
            dec.negotiate_output()?;

            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(dec)
        }
    }

    /// Feed one access unit (config or frame); returns any frames it produced.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<Frame>> {
        let sample = self.make_input_sample(au)?;
        let mut frames = Vec::new();
        unsafe {
            // ProcessInput may reject input while output is pending; drain then retry.
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        self.drain(&mut frames)?;
                    }
                    Err(e) => return Err(anyhow!("ProcessInput: {e}")),
                }
            }
            self.drain(&mut frames)?;
        }
        Ok(frames)
    }

    fn make_input_sample(&mut self, au: &[u8]) -> Result<IMFSample> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(au.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.pts)?;
            sample.SetSampleDuration(333_667)?; // ~30fps hint; not authoritative
            self.pts += 333_667;
            Ok(sample)
        }
    }

    unsafe fn drain(&mut self, frames: &mut Vec<Frame>) -> Result<()> {
        loop {
            // Reset our output buffer before each call.
            if let Some(s) = &self.out_sample {
                if let Ok(buf) = unsafe { s.GetBufferByIndex(0) } {
                    let _ = unsafe { buf.SetCurrentLength(0) };
                }
            }

            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.out_sample.clone()),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let res = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
            // Release the reference we cloned into the data buffer.
            unsafe { ManuallyDrop::drop(&mut out[0].pSample) };

            match res {
                Ok(()) => {
                    if let Some(frame) = self.read_output_frame()? {
                        frames.push(frame);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate_output()?;
                }
                Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
            }
        }
        Ok(())
    }

    /// Pick NV12 output, read geometry, (re)allocate the output sample.
    fn negotiate_output(&mut self) -> Result<()> {
        unsafe {
            let mut idx = 0u32;
            let chosen = loop {
                match self.transform.GetOutputAvailableType(0, idx) {
                    Ok(t) => {
                        if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                            break t;
                        }
                        idx += 1;
                    }
                    Err(_) => bail!("decoder offered no NV12 output type"),
                }
            };
            self.transform.SetOutputType(0, &chosen, 0)?;

            let fs = chosen.GetUINT64(&MF_MT_FRAME_SIZE)?;
            self.coded_w = (fs >> 32) as u32;
            self.coded_h = (fs & 0xFFFF_FFFF) as u32;
            self.stride = match chosen.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(s) => (s as i32).unsigned_abs() as usize,
                Err(_) => self.coded_w as usize,
            };
            // Clamp the visible region to what scrcpy promised (strips MB padding).
            self.vis_w = self.coded_w.min(self.expect_w.max(1));
            self.vis_h = self.coded_h.min(self.expect_h.max(1));

            let info = self.transform.GetOutputStreamInfo(0)?;
            let buffer = MFCreateMemoryBuffer(info.cbSize.max(self.stride as u32 * self.coded_h))?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.out_sample = Some(sample);
            Ok(())
        }
    }

    fn read_output_frame(&mut self) -> Result<Option<Frame>> {
        let Some(sample) = &self.out_sample else {
            return Ok(None);
        };
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize);

            let frame = nv12_to_rgba(
                data,
                self.stride,
                self.coded_h as usize,
                self.vis_w as usize,
                self.vis_h as usize,
            );
            buffer.Unlock()?;
            Ok(frame)
        }
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// NV12 -> RGBA (BT.601 limited range). `stride`/`coded_h` describe the source
/// buffer layout; `vw`/`vh` is the visible top-left region we keep.
///
/// The per-pixel conversion is the heaviest CPU cost in the pipeline, so we
/// split the output rows across worker threads: at high frame-rates a single
/// core can't keep up with a ~2 MP frame, and any backlog turns into latency.
fn nv12_to_rgba(
    data: &[u8],
    stride: usize,
    coded_h: usize,
    vw: usize,
    vh: usize,
) -> Option<Frame> {
    let uv_offset = stride * coded_h;
    if vw == 0 || vh == 0 || data.len() < uv_offset + stride * (coded_h / 2) {
        return None;
    }
    let mut rgba = vec![0u8; vw * vh * 4];

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let rows_per = vh.div_ceil(threads);

    std::thread::scope(|s| {
        for (chunk_idx, out_chunk) in rgba.chunks_mut(rows_per * vw * 4).enumerate() {
            let y_start = chunk_idx * rows_per;
            let data = &*data;
            s.spawn(move || {
                let rows = out_chunk.len() / (vw * 4);
                for r in 0..rows {
                    let y = y_start + r;
                    let y_row = y * stride;
                    let uv_row = uv_offset + (y / 2) * stride;
                    let out_row = r * vw * 4;
                    for x in 0..vw {
                        let yy = data[y_row + x] as i32;
                        let uv_x = x & !1;
                        let u = data[uv_row + uv_x] as i32;
                        let v = data[uv_row + uv_x + 1] as i32;

                        let c = yy - 16;
                        let d = u - 128;
                        let e = v - 128;
                        let r0 = (298 * c + 409 * e + 128) >> 8;
                        let g0 = (298 * c - 100 * d - 208 * e + 128) >> 8;
                        let b0 = (298 * c + 516 * d + 128) >> 8;

                        let o = out_row + x * 4;
                        out_chunk[o] = r0.clamp(0, 255) as u8;
                        out_chunk[o + 1] = g0.clamp(0, 255) as u8;
                        out_chunk[o + 2] = b0.clamp(0, 255) as u8;
                        out_chunk[o + 3] = 255;
                    }
                }
            });
        }
    });
    Some(Frame { width: vw as u32, height: vh as u32, rgba })
}
