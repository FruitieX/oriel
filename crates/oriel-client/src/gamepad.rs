//! Gamepad passthrough: local controllers (gilrs) are sent to the host as
//! XInput-style states while the viewer window is focused; the host's
//! rumble plays on them.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gilrs::{
    Axis, Button, EventType, GamepadId, Gilrs,
    ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Replay, Ticks},
};
use oriel_proto::{ClientMsg, Datagram, GamepadState, gamepad as b};
use tokio::sync::mpsc;

/// Rumble requests from the host: (pad, strong, weak).
pub type RumbleSender = crossbeam_channel::Sender<(u8, u16, u16)>;

/// Resend states this often, so a lost datagram is soon corrected.
const RESEND: Duration = Duration::from_millis(100);

struct Pad {
    index: u8,
    seq: u32,
    sent: Option<GamepadState>,
    rumble: Option<Effect>,
}

/// Start forwarding gamepads. Stops when the connection closes.
pub fn spawn(
    conn: quinn::Connection,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    focused: Arc<AtomicBool>,
) -> RumbleSender {
    let (rumble_tx, rumble_rx) = crossbeam_channel::unbounded::<(u8, u16, u16)>();
    std::thread::Builder::new()
        .name("oriel-gamepads".into())
        .spawn(move || {
            let mut gilrs = match Gilrs::new() {
                Ok(g) => g,
                Err(e) => {
                    tracing::info!("gamepads unavailable: {e}");
                    return;
                }
            };
            let mut pads: HashMap<GamepadId, Pad> = HashMap::new();
            let mut last_resend = Instant::now();
            while conn.close_reason().is_none() {
                // Wake often enough to apply rumble promptly.
                let event = gilrs.next_event_blocking(Some(Duration::from_millis(20)));
                if let Some(ev) = &event {
                    match ev.event {
                        EventType::Connected => {
                            let gp = gilrs.gamepad(ev.id);
                            tracing::info!(name = gp.name(), "gamepad connected");
                        }
                        EventType::Disconnected => {
                            if let Some(pad) = pads.remove(&ev.id) {
                                let _ = ctl.send(ClientMsg::GamepadRemoved { pad: pad.index });
                            }
                        }
                        _ => {}
                    }
                }
                // Drain the rest without blocking.
                while gilrs.next_event().is_some() {}

                let focused = focused.load(Ordering::Relaxed);
                let resend = last_resend.elapsed() >= RESEND;
                if resend {
                    last_resend = Instant::now();
                }
                let connected: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
                for id in connected {
                    // Unfocused: release everything once, then stay quiet.
                    let state = if focused {
                        read_state(&gilrs, id)
                    } else {
                        GamepadState::default()
                    };
                    let pad = match pads.get_mut(&id) {
                        Some(p) => p,
                        None => {
                            if !focused || state == GamepadState::default() {
                                continue;
                            }
                            let used: Vec<u8> = pads.values().map(|p| p.index).collect();
                            let index = (0..8u8).find(|i| !used.contains(i)).unwrap_or(7);
                            pads.entry(id).or_insert(Pad {
                                index,
                                seq: 0,
                                sent: None,
                                rumble: None,
                            })
                        }
                    };
                    let changed = pad.sent != Some(state);
                    if !changed && !(resend && focused) {
                        continue;
                    }
                    pad.seq = pad.seq.wrapping_add(1);
                    pad.sent = Some(state);
                    let dg = Datagram::Gamepad {
                        pad: pad.index,
                        seq: pad.seq,
                        state,
                    };
                    if let Ok(bytes) = oriel_proto::encode(&dg) {
                        let _ = conn.send_datagram(bytes.into());
                    }
                }

                while let Ok((index, strong, weak)) = rumble_rx.try_recv() {
                    let Some((&id, pad)) = pads.iter_mut().find(|(_, p)| p.index == index) else {
                        continue;
                    };
                    pad.rumble = None; // dropping an effect stops it
                    if strong == 0 && weak == 0 || !gilrs.gamepad(id).is_ff_supported() {
                        continue;
                    }
                    let scheduling = Replay {
                        play_for: Ticks::from_ms(60_000),
                        ..Default::default()
                    };
                    let effect = EffectBuilder::new()
                        .add_effect(BaseEffect {
                            kind: BaseEffectType::Strong { magnitude: strong },
                            scheduling,
                            ..Default::default()
                        })
                        .add_effect(BaseEffect {
                            kind: BaseEffectType::Weak { magnitude: weak },
                            scheduling,
                            ..Default::default()
                        })
                        .gamepads(&[id])
                        .finish(&mut gilrs);
                    match effect {
                        Ok(e) => {
                            let _ = e.play();
                            pad.rumble = Some(e);
                        }
                        Err(e) => tracing::debug!("rumble failed: {e}"),
                    }
                }
            }
        })
        .expect("spawn gamepad thread");
    rumble_tx
}

fn read_state(gilrs: &Gilrs, id: GamepadId) -> GamepadState {
    let gp = gilrs.gamepad(id);
    let mut buttons = 0u16;
    for (button, bit) in [
        (Button::South, b::A),
        (Button::East, b::B),
        (Button::West, b::X),
        (Button::North, b::Y),
        (Button::LeftTrigger, b::LEFT_SHOULDER),
        (Button::RightTrigger, b::RIGHT_SHOULDER),
        (Button::Select, b::BACK),
        (Button::Start, b::START),
        (Button::Mode, b::GUIDE),
        (Button::LeftThumb, b::LEFT_THUMB),
        (Button::RightThumb, b::RIGHT_THUMB),
        (Button::DPadUp, b::DPAD_UP),
        (Button::DPadDown, b::DPAD_DOWN),
        (Button::DPadLeft, b::DPAD_LEFT),
        (Button::DPadRight, b::DPAD_RIGHT),
    ] {
        if gp.is_pressed(button) {
            buttons |= bit;
        }
    }
    let stick = |axis| (gp.value(axis).clamp(-1.0, 1.0) * 32767.0) as i16;
    let trigger = |button| {
        gp.button_data(button)
            .map_or(0, |d| (d.value().clamp(0.0, 1.0) * 255.0) as u8)
    };
    GamepadState {
        buttons,
        left_trigger: trigger(Button::LeftTrigger2),
        right_trigger: trigger(Button::RightTrigger2),
        lx: stick(Axis::LeftStickX),
        ly: stick(Axis::LeftStickY),
        rx: stick(Axis::RightStickX),
        ry: stick(Axis::RightStickY),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use evdev::{
        AbsInfo, AbsoluteAxisCode as Abs, AttributeSet, BusType, EventType, InputEvent, InputId,
        KeyCode, UinputAbsSetup, uinput::VirtualDevice,
    };

    /// Reads a fake Xbox 360 controller. Needs /dev/uinput and the event
    /// devices (run as root); skipped otherwise.
    #[test]
    fn reads_controller_state() {
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            eprintln!("skipping: /dev/uinput is not writable");
            return;
        }
        let keys = AttributeSet::from_iter([
            KeyCode::BTN_SOUTH,
            KeyCode::BTN_EAST,
            KeyCode::BTN_NORTH,
            KeyCode::BTN_WEST,
            KeyCode::BTN_TL,
            KeyCode::BTN_TR,
            KeyCode::BTN_SELECT,
            KeyCode::BTN_START,
            KeyCode::BTN_MODE,
            KeyCode::BTN_THUMBL,
            KeyCode::BTN_THUMBR,
        ]);
        let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
        let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let mut builder = VirtualDevice::builder()
            .unwrap()
            .name("Microsoft X-Box 360 pad")
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)
            .unwrap();
        for (code, info) in [
            (Abs::ABS_X, stick),
            (Abs::ABS_Y, stick),
            (Abs::ABS_RX, stick),
            (Abs::ABS_RY, stick),
            (Abs::ABS_Z, trigger),
            (Abs::ABS_RZ, trigger),
            (Abs::ABS_HAT0X, hat),
            (Abs::ABS_HAT0Y, hat),
        ] {
            builder = builder
                .with_absolute_axis(&UinputAbsSetup::new(code, info))
                .unwrap();
        }
        let mut device = builder.build().unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let mut gilrs = Gilrs::new().unwrap();
        let abs = |code: Abs, v| InputEvent::new(EventType::ABSOLUTE.0, code.0, v);
        device
            .emit(&[
                InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 1),
                abs(Abs::ABS_X, 16000),
                abs(Abs::ABS_Y, -32767),
                abs(Abs::ABS_RZ, 255),
                abs(Abs::ABS_HAT0Y, 1),
            ])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found = None;
        while Instant::now() < deadline && found.is_none() {
            gilrs.next_event_blocking(Some(Duration::from_millis(50)));
            found = gilrs
                .gamepads()
                .find(|(_, g)| g.vendor_id() == Some(0x045e) && g.is_pressed(Button::South))
                .map(|(id, _)| id);
        }
        let id = found.expect("gilrs sees the fake controller with A pressed");
        while gilrs.next_event().is_some() {}
        let state = read_state(&gilrs, id);
        eprintln!("{state:?}");
        assert_eq!(state.buttons, b::A | b::DPAD_DOWN);
        assert!((15000..17000).contains(&state.lx), "lx {}", state.lx);
        assert!(state.ly > 32000, "up is positive: ly {}", state.ly);
        assert_eq!(state.right_trigger, 255);
    }
}
