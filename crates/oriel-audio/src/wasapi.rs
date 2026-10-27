//! System audio on Windows, recorded straight from WASAPI: loopback of a
//! render endpoint in its own mix format, polled (as Sunshine and OBS do).
//! cpal's loopback stream (event driven) delivered only zeros on a host
//! whose device meter showed sound.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        },
    },
    core::{GUID, HSTRING},
};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    F32,
    I16,
    I32,
}

/// A started loopback stream. Use it on the thread that opened it.
pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub rate: u32,
    pub channels: usize,
    sample: Sample,
}

/// What one `read` saw.
#[derive(Default, Clone, Copy, Debug)]
pub struct Read {
    pub frames: usize,
    /// Frames Windows flagged as silent (no stream played into the mix).
    pub silent_frames: usize,
}

impl Loopback {
    /// Record what plays on endpoint `id` (a WASAPI endpoint id, with or
    /// without cpal's `wasapi:` prefix), or on the default output.
    pub fn open(id: Option<&str>) -> Result<(Self, String)> {
        // SAFETY: COM calls on this thread, in the multithreaded apartment;
        // pointers from WASAPI are read as documented and freed after use.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("create the device enumerator")?;
            let device = match id {
                Some(id) => {
                    let id = id.strip_prefix("wasapi:").unwrap_or(id);
                    enumerator
                        .GetDevice(&HSTRING::from(id))
                        .with_context(|| format!("no audio device {id}"))?
                }
                None => enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .context("no default audio output")?,
            };
            let device_id = device
                .GetId()
                .ok()
                .and_then(|p| {
                    let s = p.to_string().ok();
                    CoTaskMemFree(Some(p.0 as *const _));
                    s
                })
                .unwrap_or_default();
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("activate the audio client")?;
            let format = client.GetMixFormat().context("get the mix format")?;
            let parsed = parse_format(format);
            // 100 ms of buffer; polled every few milliseconds.
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                1_000_000,
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format as *const _));
            let (rate, channels, sample) = parsed?;
            init.context("start loopback recording")?;
            let capture: IAudioCaptureClient =
                client.GetService().context("get the capture client")?;
            client.Start().context("start the audio client")?;
            Ok((
                Self {
                    client,
                    capture,
                    rate,
                    channels,
                    sample,
                },
                device_id,
            ))
        }
    }

    /// Append everything captured since the last call to `out`
    /// (interleaved, `channels` per frame).
    pub fn read(&self, out: &mut Vec<f32>) -> Result<Read> {
        let mut read = Read::default();
        // SAFETY: each buffer is read within GetBuffer/ReleaseBuffer, as
        // many bytes as the frames and format say.
        unsafe {
            loop {
                let packet = self.capture.GetNextPacketSize()?;
                if packet == 0 {
                    return Ok(read);
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * self.channels;
                read.frames += frames as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    read.silent_frames += frames as usize;
                    out.resize(out.len() + n, 0.0);
                } else {
                    match self.sample {
                        Sample::F32 => {
                            out.extend_from_slice(std::slice::from_raw_parts(data.cast::<f32>(), n))
                        }
                        Sample::I16 => out.extend(
                            std::slice::from_raw_parts(data.cast::<i16>(), n)
                                .iter()
                                .map(|s| *s as f32 / 32768.0),
                        ),
                        Sample::I32 => out.extend(
                            std::slice::from_raw_parts(data.cast::<i32>(), n)
                                .iter()
                                .map(|s| *s as f32 / 2_147_483_648.0),
                        ),
                    }
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stopping a started client.
        let _ = unsafe { self.client.Stop() };
    }
}

/// Rate, channels and sample type of a mix format.
///
/// # Safety
/// `format` points to a valid `WAVEFORMATEX` (and the extensible form when
/// its tag says so).
unsafe fn parse_format(format: *const WAVEFORMATEX) -> Result<(u32, usize, Sample)> {
    // The structs are packed: copy fields out instead of referencing them.
    let f = unsafe { std::ptr::read_unaligned(format) };
    let (tag, bits, rate, channels) = (
        f.wFormatTag,
        f.wBitsPerSample,
        f.nSamplesPerSec,
        f.nChannels,
    );
    let float = match tag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_PCM => false,
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = unsafe { std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>()) };
            let sub = ext.SubFormat;
            if sub == SUBTYPE_IEEE_FLOAT {
                true
            } else if sub == SUBTYPE_PCM {
                false
            } else {
                bail!("unsupported mix format {sub:?}")
            }
        }
        other => bail!("unsupported mix format tag {other}"),
    };
    let sample = match (float, bits) {
        (true, 32) => Sample::F32,
        (false, 16) => Sample::I16,
        (false, 32) => Sample::I32,
        _ => bail!(
            "unsupported mix format: {bits}-bit {}",
            if float { "float" } else { "integer" }
        ),
    };
    Ok((rate, channels as usize, sample))
}
