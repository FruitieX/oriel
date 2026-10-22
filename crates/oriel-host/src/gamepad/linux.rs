//! Xbox 360 pads through uinput, looking like the kernel's xpad driver so
//! games and SDL map them without configuration. Needs write access to
//! /dev/uinput (NixOS: `hardware.uinput.enable` and the `uinput` group).

use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode,
    FFEffectKind, InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup, uinput::VirtualDevice,
};
use oriel_proto::{GamepadState, gamepad as b};

use super::{RumbleFn, VirtualPad};

const BUTTONS: [(u16, KeyCode); 11] = [
    (b::A, KeyCode::BTN_SOUTH),
    (b::B, KeyCode::BTN_EAST),
    (b::X, KeyCode::BTN_NORTH),
    (b::Y, KeyCode::BTN_WEST),
    (b::LEFT_SHOULDER, KeyCode::BTN_TL),
    (b::RIGHT_SHOULDER, KeyCode::BTN_TR),
    (b::BACK, KeyCode::BTN_SELECT),
    (b::START, KeyCode::BTN_START),
    (b::GUIDE, KeyCode::BTN_MODE),
    (b::LEFT_THUMB, KeyCode::BTN_THUMBL),
    (b::RIGHT_THUMB, KeyCode::BTN_THUMBR),
];

/// The pad lives on its own thread, which also answers force feedback
/// requests from games.
struct Pad {
    states: Option<crossbeam_channel::Sender<GamepadState>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VirtualPad for Pad {
    fn update(&mut self, state: &GamepadState) -> Result<()> {
        self.states
            .as_ref()
            .context("gamepad thread ended")?
            .send(*state)
            .ok()
            .context("gamepad thread ended")
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        self.states = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub(super) fn create(pad: u8, rumble: RumbleFn) -> Result<Box<dyn VirtualPad>> {
    let mut device = build().context("create a uinput gamepad (is /dev/uinput writable?)")?;
    // Non-blocking, so the thread can poll for force feedback requests.
    unsafe {
        let fd = device.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let (tx, rx) = crossbeam_channel::unbounded::<GamepadState>();
    let thread = std::thread::Builder::new()
        .name(format!("oriel-pad-{pad}"))
        .spawn(move || run(&mut device, pad, rx, rumble))?;
    Ok(Box::new(Pad {
        states: Some(tx),
        thread: Some(thread),
    }))
}

fn build() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for (_, key) in BUTTONS {
        keys.insert(key);
    }
    let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
    let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
    let mut builder = VirtualDevice::builder()?
        .name("Microsoft X-Box 360 pad")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
        .with_keys(&keys)?;
    for (code, info) in [
        (AbsoluteAxisCode::ABS_X, stick),
        (AbsoluteAxisCode::ABS_Y, stick),
        (AbsoluteAxisCode::ABS_RX, stick),
        (AbsoluteAxisCode::ABS_RY, stick),
        (AbsoluteAxisCode::ABS_Z, trigger),
        (AbsoluteAxisCode::ABS_RZ, trigger),
        (AbsoluteAxisCode::ABS_HAT0X, hat),
        (AbsoluteAxisCode::ABS_HAT0Y, hat),
    ] {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(code, info))?;
    }
    Ok(builder
        .with_ff(&AttributeSet::from_iter([FFEffectCode::FF_RUMBLE]))?
        .with_ff_effects_max(16)
        .build()?)
}

fn events(s: &GamepadState) -> Vec<InputEvent> {
    let key = |code: KeyCode, on: bool| InputEvent::new(EventType::KEY.0, code.0, on as i32);
    let abs = |code: AbsoluteAxisCode, v: i32| InputEvent::new(EventType::ABSOLUTE.0, code.0, v);
    let pressed = |bit: u16| s.buttons & bit != 0;
    let hat = |neg: u16, pos: u16| pressed(pos) as i32 - pressed(neg) as i32;
    let mut out: Vec<InputEvent> = BUTTONS
        .iter()
        .map(|&(bit, code)| key(code, pressed(bit)))
        .collect();
    out.extend([
        abs(AbsoluteAxisCode::ABS_X, s.lx as i32),
        // evdev's Y axes point down.
        abs(AbsoluteAxisCode::ABS_Y, -(s.ly as i32).clamp(-32767, 32767)),
        abs(AbsoluteAxisCode::ABS_RX, s.rx as i32),
        abs(
            AbsoluteAxisCode::ABS_RY,
            -(s.ry as i32).clamp(-32767, 32767),
        ),
        abs(AbsoluteAxisCode::ABS_Z, s.left_trigger as i32),
        abs(AbsoluteAxisCode::ABS_RZ, s.right_trigger as i32),
        abs(
            AbsoluteAxisCode::ABS_HAT0X,
            hat(b::DPAD_LEFT, b::DPAD_RIGHT),
        ),
        abs(AbsoluteAxisCode::ABS_HAT0Y, hat(b::DPAD_UP, b::DPAD_DOWN)),
    ]);
    out
}

fn run(
    device: &mut VirtualDevice,
    pad: u8,
    states: crossbeam_channel::Receiver<GamepadState>,
    rumble: RumbleFn,
) {
    // Uploaded rumble effects: id → (strong, weak, length).
    let mut effects: HashMap<i16, (u16, u16, Duration)> = HashMap::new();
    let mut playing_until: Option<Instant> = None;
    loop {
        match states.recv_timeout(Duration::from_millis(4)) {
            Ok(state) => {
                if let Err(e) = device.emit(&events(&state)) {
                    tracing::debug!("gamepad emit failed: {e}");
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if playing_until.is_some_and(|t| Instant::now() >= t) {
            playing_until = None;
            rumble(pad, 0, 0);
        }
        let Ok(events) = device.fetch_events() else {
            continue;
        };
        let events: Vec<InputEvent> = events.collect();
        for event in events {
            match event.destructure() {
                EventSummary::UInput(ev, UInputCode::UI_FF_UPLOAD, ..) => {
                    let Ok(mut upload) = device.process_ff_upload(ev) else {
                        continue;
                    };
                    let effect = upload.effect();
                    // The kernel has already chosen the effect's id.
                    let id = upload.effect_id();
                    let (strong, weak) = match effect.kind {
                        FFEffectKind::Rumble {
                            strong_magnitude,
                            weak_magnitude,
                        } => (strong_magnitude, weak_magnitude),
                        _ => (0, 0),
                    };
                    effects.insert(
                        id,
                        (
                            strong,
                            weak,
                            Duration::from_millis(effect.replay.length as u64),
                        ),
                    );
                    upload.set_retval(0);
                }
                EventSummary::UInput(ev, UInputCode::UI_FF_ERASE, ..) => {
                    if let Ok(erase) = device.process_ff_erase(ev) {
                        effects.remove(&(erase.effect_id() as i16));
                    }
                }
                EventSummary::ForceFeedback(_, code, value) if code.0 < 16 => {
                    let Some(&(strong, weak, length)) = effects.get(&(code.0 as i16)) else {
                        continue;
                    };
                    if value > 0 {
                        rumble(pad, strong, weak);
                        playing_until = (!length.is_zero()).then(|| Instant::now() + length);
                    } else {
                        rumble(pad, 0, 0);
                        playing_until = None;
                    }
                }
                _ => {}
            }
        }
    }
    rumble(pad, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::{FFEffectData, FFReplay, FFTrigger};

    /// Needs write access to /dev/uinput (run as root); skipped otherwise.
    #[test]
    fn state_and_rumble_roundtrip() {
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            eprintln!("skipping: /dev/uinput is not writable");
            return;
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut pad = create(
            3,
            std::sync::Arc::new(move |pad, strong, weak| {
                let _ = tx.send((pad, strong, weak));
            }),
        )
        .unwrap();
        // Let udev create the device node.
        std::thread::sleep(Duration::from_millis(500));
        let state = GamepadState {
            buttons: b::A | b::DPAD_LEFT,
            right_trigger: 200,
            lx: 12000,
            ly: 30000,
            ..Default::default()
        };
        pad.update(&state).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let (_, mut device) = evdev::enumerate()
            .find(|(_, d)| {
                d.name() == Some("Microsoft X-Box 360 pad")
                    && d.input_id().product() == 0x028e
                    && d.supported_ff().is_some()
            })
            .expect("virtual pad visible");
        let keys = device.get_key_state().unwrap();
        assert!(keys.contains(KeyCode::BTN_SOUTH));
        assert!(!keys.contains(KeyCode::BTN_EAST));
        let abs = device.get_abs_state().unwrap();
        assert_eq!(abs[AbsoluteAxisCode::ABS_X.0 as usize].value, 12000);
        assert_eq!(abs[AbsoluteAxisCode::ABS_Y.0 as usize].value, -30000);
        assert_eq!(abs[AbsoluteAxisCode::ABS_RZ.0 as usize].value, 200);
        assert_eq!(abs[AbsoluteAxisCode::ABS_HAT0X.0 as usize].value, -1);

        // A game plays a 200 ms rumble.
        let mut effect = device
            .upload_ff_effect(FFEffectData {
                direction: 0,
                trigger: FFTrigger::default(),
                replay: FFReplay {
                    length: 200,
                    delay: 0,
                },
                kind: FFEffectKind::Rumble {
                    strong_magnitude: 0x8000,
                    weak_magnitude: 0x4000,
                },
            })
            .unwrap();
        effect.play(1).unwrap();
        let timeout = Duration::from_secs(1);
        assert_eq!(rx.recv_timeout(timeout).unwrap(), (3, 0x8000, 0x4000));
        let started = Instant::now();
        assert_eq!(rx.recv_timeout(timeout).unwrap(), (3, 0, 0));
        assert!(started.elapsed() >= Duration::from_millis(150));
    }
}
