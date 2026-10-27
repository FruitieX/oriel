//! System audio on Windows, recorded straight from WASAPI.
//!
//! By default every process's sound except oriel's own (process loopback,
//! Windows 10 2004 and later): it is taken from the applications' streams,
//! wherever they play. Loopback of an output device returned nothing but
//! zeros for VB-Audio's virtual devices (Voicemeeter, VB-Cable) with music
//! playing on them, which Parsec still streamed. A chosen device (or older
//! Windows) is recorded with endpoint loopback in its mix format.

use anyhow::{Context, Result, bail};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
        },
        Media::Audio::{
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
            AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
            ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler,
            IActivateAudioInterfaceCompletionHandler_Impl,
            PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        },
        System::{
            Com::{
                BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, StructuredStorage::PROPVARIANT,
            },
            Threading::CreateEventW,
            Variant::VT_BLOB,
        },
    },
    core::{GUID, HSTRING, IUnknown, Interface, Ref, implement},
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
    /// Set for event-driven streams (process loopback needs one); the
    /// stream is polled either way.
    event: Option<HANDLE>,
}

/// Hands the activated interface back from `ActivateAudioInterfaceAsync`.
/// It also owns the activation parameters: Windows keeps the handler alive
/// until it calls back, so they cannot be freed under a slow activation.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated {
    tx: std::sync::mpsc::Sender<windows::core::Result<IUnknown>>,
    _params: Box<AUDIOCLIENT_ACTIVATION_PARAMS>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| {
            let operation = operation.ok()?;
            let mut hr = windows::core::HRESULT::default();
            let mut unknown: Option<IUnknown> = None;
            // SAFETY: both out-pointers are valid for the call.
            unsafe { operation.GetActivateResult(&mut hr, &mut unknown)? };
            hr.ok()?;
            unknown.ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
        })();
        let _ = self.tx.send(result);
        Ok(())
    }
}

/// What one `read` saw.
#[derive(Default, Clone, Copy, Debug)]
pub struct Read {
    pub frames: usize,
    /// Frames Windows flagged as silent (no stream played into the mix).
    pub silent_frames: usize,
}

impl Loopback {
    /// Record what every process plays except this one (and its children),
    /// on any device, as 48 kHz stereo float.
    pub fn open_processes() -> Result<Self> {
        // SAFETY: COM calls on this thread, in the multithreaded apartment.
        // The activation parameters outlive the (waited for) activation.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: std::process::id(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            });
            let blob = BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: (&*params as *const AUDIOCLIENT_ACTIVATION_PARAMS)
                    .cast_mut()
                    .cast(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let handler: IActivateAudioInterfaceCompletionHandler = Activated {
                tx,
                _params: params,
            }
            .into();
            // A PROPVARIANT pointing at the parameters (owned by the
            // handler). Never dropped: windows-rs drops one with
            // PropVariantClear, which would free the parameters with COM's
            // allocator (heap corruption, the host crashed).
            let mut prop = std::mem::ManuallyDrop::new(PROPVARIANT::default());
            {
                let inner = &mut *prop.Anonymous.Anonymous;
                inner.vt = VT_BLOB;
                inner.Anonymous.blob = blob;
            }
            tracing::debug!(pid = std::process::id(), "activating process loopback");
            let _operation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*prop),
                &handler,
            )
            .context("process loopback is not available (Windows 10 2004 or later)")?;
            let client: IAudioClient = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .context("process loopback did not activate")?
                .context("activate process loopback")?
                .cast()?;
            tracing::debug!("process loopback activated");
            const RATE: u32 = 48_000;
            const CHANNELS: u16 = 2;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: CHANNELS,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
                nBlockAlign: CHANNELS * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    1_000_000,
                    0,
                    &format,
                    None,
                )
                .context("start process loopback")?;
            tracing::debug!("process loopback initialized");
            let event = CreateEventW(None, false, false, None).context("create an event")?;
            let loopback = Self {
                capture: client.GetService().context("get the capture client")?,
                client,
                rate: RATE,
                channels: CHANNELS as usize,
                sample: Sample::F32,
                event: Some(event),
            };
            loopback
                .client
                .SetEventHandle(event)
                .context("set the event")?;
            loopback.client.Start().context("start the audio client")?;
            Ok(loopback)
        }
    }

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
                    event: None,
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
        // SAFETY: stopping a started client, then closing our event.
        unsafe {
            let _ = self.client.Stop();
            if let Some(event) = self.event {
                let _ = CloseHandle(event);
            }
        }
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

/// Mutes the default output while it lives, then restores its previous
/// state. A thread owns the COM object (they are not `Send`), so this can be
/// held by an async task.
pub struct Mute {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mute {
    pub fn default_output() -> Result<Self> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("oriel-mute".into())
            .spawn(move || {
                // SAFETY: COM calls on this thread, in the multithreaded
                // apartment.
                let volume = unsafe {
                    (|| -> Result<(IAudioEndpointVolume, bool)> {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                        let enumerator: IMMDeviceEnumerator =
                            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                        let device = enumerator
                            .GetDefaultAudioEndpoint(eRender, eConsole)
                            .context("no default audio output")?;
                        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
                        let was_muted = volume.GetMute()?.as_bool();
                        volume.SetMute(true, std::ptr::null())?;
                        Ok((volume, was_muted))
                    })()
                };
                let (volume, was_muted) = match volume {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                // Until dropped (the sender goes away).
                let _ = stopped.recv();
                if !was_muted {
                    // SAFETY: as above.
                    let _ = unsafe { volume.SetMute(false, std::ptr::null()) };
                }
            })?;
        ready_rx.recv().context("mute thread exited")??;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Mute {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
