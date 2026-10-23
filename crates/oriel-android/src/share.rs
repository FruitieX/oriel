//! Sharing the phone: the host server, fed by the Java screen share
//! (MediaProjection into a MediaCodec encoder) and controlled through the
//! accessibility service.

use std::sync::Arc;

use anyhow::Result;
use jni::objects::JValue;
use oriel_client::android::with_activity;
use oriel_host::input::android::{Gestures, Global};
use oriel_media::share;
use oriel_net::Paths;
use oriel_proto::Codec;

fn call(method: &str, sig: &str, args: &[JValue]) {
    with_activity(|env, activity| env.call_method(activity, method, sig, args).map(|_| ()));
}

/// The encoder on the Java side.
struct JavaShare;

impl share::Control for JavaShare {
    fn start(&self, codec: Codec, width: u32, height: u32, fps: u32, kbps: u32) -> Result<()> {
        let mime = match codec {
            Codec::Hevc => "video/hevc",
            _ => "video/avc",
        };
        let ok = with_activity(|env, activity| {
            let mime = env.new_string(mime)?;
            env.call_method(
                activity,
                "startEncoder",
                "(Ljava/lang/String;IIII)Z",
                &[
                    JValue::Object(&mime),
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    JValue::Int(fps as i32),
                    JValue::Int(kbps as i32),
                ],
            )?
            .z()
        });
        if ok != Some(true) {
            anyhow::bail!("the phone's encoder did not start");
        }
        Ok(())
    }

    fn stop(&self) {
        call("stopEncoder", "()V", &[]);
    }

    fn keyframe(&self) {
        call("requestKeyframe", "()V", &[]);
    }

    fn bitrate(&self, kbps: u32) {
        call("setBitrate", "(I)V", &[JValue::Int(kbps as i32)]);
    }
}

/// The accessibility service.
struct JavaGestures;

impl Gestures for JavaGestures {
    fn tap(&self, x: f32, y: f32, long: bool) {
        call(
            "a11yTap",
            "(FFZ)V",
            &[
                JValue::Float(x),
                JValue::Float(y),
                JValue::Bool(long.into()),
            ],
        );
    }

    fn swipe(&self, path: &[(f32, f32)], ms: u32) {
        with_activity(|env, activity| {
            let flat: Vec<f32> = path.iter().flat_map(|(x, y)| [*x, *y]).collect();
            let arr = env.new_float_array(flat.len() as i32)?;
            env.set_float_array_region(&arr, 0, &flat)?;
            env.call_method(
                activity,
                "a11ySwipe",
                "([FI)V",
                &[JValue::Object(&arr), JValue::Int(ms as i32)],
            )
            .map(|_| ())
        });
    }

    fn global(&self, action: Global) {
        let n = match action {
            Global::Back => 1,
            Global::Home => 2,
            Global::Recents => 3,
        };
        call("a11yGlobal", "(I)V", &[JValue::Int(n)]);
    }

    fn text(&self, text: &str) {
        with_activity(|env, activity| {
            let s = env.new_string(text)?;
            env.call_method(
                activity,
                "a11yText",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&s)],
            )
            .map(|_| ())
        });
    }

    fn backspace(&self) {
        call("a11yBackspace", "()V", &[]);
    }

    fn enter(&self) {
        call("a11yEnter", "()V", &[]);
    }
}

/// Hosting on the phone, as the start window sees it. The host server runs
/// while the screen is shared, however sharing started (the start window,
/// or a wake-up's notification).
pub struct PhoneHost {
    options: oriel_host::HostOptions,
    rt: tokio::runtime::Runtime,
    control: oriel_host::HostControl,
    task: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl PhoneHost {
    pub fn new(paths: Paths) -> Result<Self> {
        share::set_control(Some(Arc::new(JavaShare)));
        oriel_host::input::android::set_gestures(Some(Arc::new(JavaGestures)));
        let host = Self {
            control: oriel_host::HostControl::new(None),
            options: oriel_host::HostOptions {
                listen: std::net::SocketAddr::from(([0, 0, 0, 0], oriel_proto::DEFAULT_PORT)),
                name: oriel_client::connection::device_name(),
                source: oriel_host::SourceKind::Auto,
                pair_pin: None,
                encoder: None,
                codec: None,
                default_bitrate_kbps: 20_000,
            },
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
            task: Default::default(),
        };
        // Follow the screen share.
        let (task, options, control) = (
            host.task.clone(),
            host.options.clone(),
            host.control.clone(),
        );
        host.rt.spawn(async move {
            loop {
                let sharing = share::screen_size().is_some();
                {
                    let mut t = task.lock().unwrap();
                    let running = t.as_ref().is_some_and(|t| !t.is_finished());
                    if sharing && !running {
                        let (paths, options, control) =
                            (paths.clone(), options.clone(), control.clone());
                        *t = Some(tokio::spawn(async move {
                            if let Err(e) = oriel_host::run_with(paths, options, control).await {
                                tracing::error!("host stopped: {e:#}");
                            }
                        }));
                    } else if !sharing && let Some(old) = t.take() {
                        old.abort();
                        control.stop_pairing();
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        });
        Ok(host)
    }
}

impl oriel_client::launcher::HostControl for PhoneHost {
    fn start(&mut self) -> Result<()> {
        // Asks for the screen (Android's consent dialog) and, the first
        // time, for the accessibility service that controls the phone.
        call("requestScreenShare", "()V", &[]);
        Ok(())
    }

    fn stop(&mut self) {
        call("stopScreenShare", "()V", &[]);
    }

    fn is_running(&self) -> bool {
        share::screen_size().is_some()
    }

    fn description(&self) -> String {
        let control = with_activity(|env, activity| {
            env.call_method(activity, "a11yEnabled", "()Z", &[])?.z()
        })
        .unwrap_or(false);
        let mut text = match share::screen_size() {
            Some((w, h)) => format!("Shares this phone's screen ({w}×{h})."),
            None => "Shares this phone's screen; Android asks first.".to_string(),
        };
        if !control {
            text.push_str(
                " To be controlled too, turn on oriel under Settings → Accessibility \
                 (sharing asks for it).",
            );
        }
        text
    }

    fn host_name(&self) -> String {
        self.options.name.clone()
    }

    fn clients(&self) -> Vec<oriel_client::launcher::SharedClient> {
        self.control
            .status()
            .clients
            .into_iter()
            .map(|c| oriel_client::launcher::SharedClient {
                id: c.id,
                name: c.name,
                address: c.address.ip().to_string(),
                codec: c.codec,
            })
            .collect()
    }

    fn kick(&self, id: usize) {
        self.control.kick(id);
    }

    fn allow_pairing(&mut self) -> Option<String> {
        Some(self.control.allow_pairing())
    }

    fn pairing_pin(&self) -> Option<String> {
        self.control.pairing_pin()
    }

    fn stop_pairing(&mut self) {
        self.control.stop_pairing();
    }
}

/// The phone's push token: the hub wakes it with this. Told to the hub
/// while not sharing (sharing reports where the host listens instead).
pub fn push_token(paths: Paths, token: String) {
    std::thread::spawn(move || {
        let Some(account) = oriel_net::hub::account(&paths) else {
            return;
        };
        if share::screen_size().is_some() {
            return;
        }
        let presence = oriel_proto::hub::Presence {
            name: oriel_client::connection::device_name(),
            platform: std::env::consts::OS.into(),
            host: None,
            push_token: Some(token),
        };
        if let Err(e) = account.presence(&presence) {
            tracing::warn!("cannot give the hub the push token: {e:#}");
        }
    });
}
