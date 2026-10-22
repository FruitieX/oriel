//! Linux system sound: a PipeWire capture stream on the default output
//! (`stream.capture.sink`), converted by PipeWire to 48 kHz stereo f32.

use anyhow::Result;
use pipewire as pw;
use pw::spa;

use crate::{CHANNELS, SAMPLE_RATE};

/// Stops the capture when dropped.
pub struct SinkCapture {
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SinkCapture {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn capture_output(tx: crossbeam_channel::Sender<Vec<f32>>) -> Result<SinkCapture> {
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let thread = std::thread::Builder::new()
        .name("oriel-pw-audio".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                pw::init();
                let mainloop = pw::main_loop::MainLoopRc::new(None)?;
                let context = pw::context::ContextRc::new(&mainloop, None)?;
                let core = context.connect_rc(None)?;
                let ml = mainloop.clone();
                let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
                let stream = pw::stream::StreamBox::new(
                    &core,
                    "oriel-audio",
                    pw::properties::properties! {
                        *pw::keys::MEDIA_TYPE => "Audio",
                        *pw::keys::MEDIA_CATEGORY => "Capture",
                        *pw::keys::MEDIA_ROLE => "Music",
                        "stream.capture.sink" => "true",
                        *pw::keys::NODE_LATENCY => "480/48000",
                    },
                )?;
                let _listener = stream
                    .add_local_listener_with_user_data(())
                    .process(move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else {
                            return;
                        };
                        let datas = buffer.datas_mut();
                        let Some(data) = datas.first_mut() else {
                            return;
                        };
                        let size = data.chunk().size() as usize;
                        if let Some(bytes) = data.data() {
                            let samples: Vec<f32> = bytes[..size.min(bytes.len())]
                                .chunks_exact(4)
                                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                                .collect();
                            let _ = tx.try_send(samples);
                        }
                    })
                    .register()?;
                let mut info = spa::param::audio::AudioInfoRaw::new();
                info.set_format(spa::param::audio::AudioFormat::F32LE);
                info.set_rate(SAMPLE_RATE);
                info.set_channels(CHANNELS as u32);
                let obj = spa::pod::Object {
                    type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                    id: spa::param::ParamType::EnumFormat.as_raw(),
                    properties: info.into(),
                };
                let bytes = spa::pod::serialize::PodSerializer::serialize(
                    std::io::Cursor::new(Vec::new()),
                    &spa::pod::Value::Object(obj),
                )
                .map_err(|e| anyhow::anyhow!("serialize format: {e:?}"))?
                .0
                .into_inner();
                let mut params = [spa::pod::Pod::from_bytes(&bytes).unwrap()];
                stream.connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT
                        | pw::stream::StreamFlags::MAP_BUFFERS
                        | pw::stream::StreamFlags::RT_PROCESS,
                    &mut params,
                )?;
                let _ = ready_tx.send(Ok(()));
                mainloop.run();
                Ok(())
            })();
            if let Err(e) = result {
                let _ = ready_tx.send(Err(e));
            }
        })?;
    ready_rx.recv()??;
    tracing::info!("capturing system audio from PipeWire");
    Ok(SinkCapture {
        quit: Some(quit_tx),
        thread: Some(thread),
    })
}
