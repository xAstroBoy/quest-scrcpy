//! AAC -> PCM -> speakers via the built-in Media Foundation AAC decoder
//! (Windows fallback when ffmpeg is absent).
//!
//! scrcpy streams raw AAC-LC (a CONFIG packet carrying the AudioSpecificConfig,
//! then raw access units). We decode with the MF AAC decoder and play through
//! cpal, resampling to the output device's rate as needed.

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::GUID;

const MF_VERSION_VAL: u32 = 0x0002_0070;
/// CLSID_CMSAACDecMFT — Microsoft AAC Audio Decoder.
const CLSID_AAC_DECODER: GUID = GUID::from_u128(0x32D186A7_218F_4C75_8876_DD77273A8999);

/// ~150 ms ceiling on queued audio (stereo @ 48 kHz) to bound A/V latency if
/// decode outruns playback — small enough to stay in sync with low-latency video.
const MAX_QUEUED_SAMPLES: usize = (48_000 * 2 * 150) / 1000;

pub struct MfPlayer {
    shared: Arc<Mutex<VecDeque<f32>>>,
    _stream: cpal::Stream,
    out_rate: u32,
    out_channels: u32,
    decoder: Option<AacDecoder>,
    // Linear-resampler state (decoder rate -> output rate), canonical stereo.
    resamp_pos: f64,
    prev: [f32; 2],
}

impl MfPlayer {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device"))?;
        let supported = device
            .default_output_config()
            .map_err(|e| anyhow!("default output config: {e}"))?;
        let out_rate = supported.sample_rate();
        let out_channels = supported.channels() as u32;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();

        let shared: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cb_buf = shared.clone();
        let err_fn = |e| eprintln!("[audio] stream error: {e}");

        // We only ever queue f32; convert in the callback for i16/u16 devices.
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config.clone(),
                move |data: &mut [f32], _| fill(&cb_buf, data),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                config.clone(),
                move |data: &mut [i16], _| {
                    let mut tmp = vec![0f32; data.len()];
                    fill(&cb_buf, &mut tmp);
                    for (o, s) in data.iter_mut().zip(tmp) {
                        *o = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                config.clone(),
                move |data: &mut [u16], _| {
                    let mut tmp = vec![0f32; data.len()];
                    fill(&cb_buf, &mut tmp);
                    for (o, s) in data.iter_mut().zip(tmp) {
                        let v = (s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32;
                        *o = v as u16;
                    }
                },
                err_fn,
                None,
            ),
            other => bail!("unsupported output sample format: {other:?}"),
        }
        .map_err(|e| anyhow!("build output stream: {e}"))?;

        stream.play().map_err(|e| anyhow!("stream.play: {e}"))?;
        eprintln!("[audio] output {out_rate} Hz x{out_channels} ({sample_format:?})");

        Ok(Self {
            shared,
            _stream: stream,
            out_rate,
            out_channels,
            decoder: None,
            resamp_pos: 0.0,
            prev: [0.0; 2],
        })
    }

    pub fn feed(&mut self, data: &[u8], is_config: bool) -> Result<()> {
        if is_config {
            // CONFIG packet = AudioSpecificConfig; (re)build the decoder.
            self.decoder = Some(AacDecoder::new(data)?);
            return Ok(());
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return Ok(()); // haven't seen the config yet
        };
        let (pcm, dec_rate, dec_ch) = decoder.decode(data)?;
        if pcm.is_empty() {
            return Ok(());
        }
        self.enqueue(&pcm, dec_rate, dec_ch);
        Ok(())
    }

    /// Resample decoded i16 frames into the shared f32 queue at the device rate.
    fn enqueue(&mut self, pcm: &[i16], dec_rate: u32, dec_ch: u32) {
        let dec_ch = dec_ch.max(1) as usize;
        let frames = pcm.len() / dec_ch;
        if frames == 0 {
            return;
        }
        // Canonical stereo source frame fetch.
        let src = |i: usize| -> [f32; 2] {
            let base = i * dec_ch;
            let l = pcm[base] as f32 / 32768.0;
            let r = if dec_ch >= 2 { pcm[base + 1] as f32 / 32768.0 } else { l };
            [l, r]
        };

        let ratio = dec_rate as f64 / self.out_rate as f64;
        let mut out = self.shared.lock().unwrap();
        // Drop backlog if we're too far ahead (keeps latency in check).
        if out.len() > MAX_QUEUED_SAMPLES {
            out.clear();
        }

        let mut pos = self.resamp_pos;
        while pos < frames as f64 {
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = if idx == 0 { self.prev } else { src(idx - 1) };
            let b = src(idx.min(frames - 1));
            let l = a[0] + (b[0] - a[0]) * frac;
            let r = a[1] + (b[1] - a[1]) * frac;
            push_frame(&mut out, l, r, self.out_channels);
            pos += ratio;
        }
        // Carry fractional position into the next buffer.
        self.resamp_pos = pos - frames as f64;
        self.prev = src(frames - 1);
    }
}

fn push_frame(out: &mut VecDeque<f32>, l: f32, r: f32, channels: u32) {
    match channels {
        1 => out.push_back((l + r) * 0.5),
        2 => {
            out.push_back(l);
            out.push_back(r);
        }
        n => {
            out.push_back(l);
            out.push_back(r);
            for _ in 2..n {
                out.push_back(0.0);
            }
        }
    }
}

fn fill(shared: &Arc<Mutex<VecDeque<f32>>>, data: &mut [f32]) {
    let mut q = shared.lock().unwrap();
    for d in data.iter_mut() {
        *d = q.pop_front().unwrap_or(0.0);
    }
}

/// Parse sample rate + channel count out of an AudioSpecificConfig.
fn parse_asc(asc: &[u8]) -> (u32, u32) {
    if asc.len() < 2 {
        return (48_000, 2);
    }
    let freq_idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
    let chan = (asc[1] >> 3) & 0x0F;
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let sr = RATES.get(freq_idx as usize).copied().unwrap_or(48_000);
    (sr, (chan as u32).clamp(1, 8))
}

struct AacDecoder {
    transform: IMFTransform,
    out_sample: Option<IMFSample>,
    sample_rate: u32,
    channels: u32,
    pts: i64,
}

impl AacDecoder {
    fn new(asc: &[u8]) -> Result<Self> {
        let (sample_rate, channels) = parse_asc(asc);
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION_VAL, MFSTARTUP_NOSOCKET)
                .map_err(|e| anyhow!("MFStartup(audio): {e}"))?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_AAC_DECODER, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| anyhow!("create AAC decoder MFT: {e}"))?;

            // Input: raw AAC with the ASC supplied as user data (12-byte
            // HEAACWAVEINFO tail + AudioSpecificConfig).
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            let mut user_data = vec![0u8; 12];
            user_data[2] = 0xFE; // wAudioProfileLevelIndication = unknown
            user_data.extend_from_slice(asc);
            input.SetBlob(&MF_MT_USER_DATA, &user_data)?;
            transform.SetInputType(0, &input, 0)?;

            // Output: 16-bit interleaved PCM.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * channels * 2)?;
            transform.SetOutputType(0, &output, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            eprintln!("[audio] AAC {sample_rate} Hz x{channels}");
            Ok(Self { transform, out_sample: None, sample_rate, channels, pts: 0 })
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<(Vec<i16>, u32, u32)> {
        let mut pcm = Vec::new();
        unsafe {
            let sample = self.make_input_sample(au)?;
            loop {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain(&mut pcm)?,
                    Err(e) => return Err(anyhow!("AAC ProcessInput: {e}")),
                }
            }
            self.drain(&mut pcm)?;
        }
        Ok((pcm, self.sample_rate, self.channels))
    }

    unsafe fn make_input_sample(&mut self, au: &[u8]) -> Result<IMFSample> {
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
            self.pts += 213_333; // ~1024 samples @ 48k in 100ns units
            Ok(sample)
        }
    }

    unsafe fn drain(&mut self, pcm: &mut Vec<i16>) -> Result<()> {
        loop {
            self.ensure_out_sample()?;
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
            unsafe { ManuallyDrop::drop(&mut out[0].pSample) };

            match res {
                Ok(()) => self.read_pcm(pcm)?,
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Re-fetch the negotiated output type and retry.
                    self.out_sample = None;
                }
                Err(e) => return Err(anyhow!("AAC ProcessOutput: {e}")),
            }
        }
        Ok(())
    }

    fn ensure_out_sample(&mut self) -> Result<()> {
        if self.out_sample.is_some() {
            return Ok(());
        }
        unsafe {
            let info = self.transform.GetOutputStreamInfo(0)?;
            let size = info.cbSize.max(self.sample_rate * self.channels * 2 / 8).max(8192);
            let buffer = MFCreateMemoryBuffer(size)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.out_sample = Some(sample);
        }
        Ok(())
    }

    fn read_pcm(&mut self, pcm: &mut Vec<i16>) -> Result<()> {
        let Some(sample) = &self.out_sample else { return Ok(()) };
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let bytes = std::slice::from_raw_parts(ptr, len as usize);
            for ch in bytes.chunks_exact(2) {
                pcm.push(i16::from_le_bytes([ch[0], ch[1]]));
            }
            buffer.Unlock()?;
        }
        Ok(())
    }
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}
