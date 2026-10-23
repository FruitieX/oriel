//! Virtual gamepads for the client's controllers: Xbox 360 pads through
//! ViGEmBus on Windows and uinput on Linux. Rumble goes back to the client.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use oriel_proto::GamepadState;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Called with (pad, strong, weak) when a game sets the rumble motors.
pub type RumbleFn = Arc<dyn Fn(u8, u16, u16) + Send + Sync>;

/// The most pads one client may plug in.
const MAX_PADS: u8 = 8;

pub enum Cmd {
    State {
        pad: u8,
        seq: u32,
        state: GamepadState,
    },
    Remove {
        pad: u8,
    },
}

trait VirtualPad {
    fn update(&mut self, state: &GamepadState) -> Result<()>;
}

/// Run the virtual pads until `cmds` closes. `notice` reports problems
/// to the user (once).
#[cfg_attr(target_os = "android", allow(unused_variables))]
pub fn spawn(
    cmds: crossbeam_channel::Receiver<Cmd>,
    rumble: RumbleFn,
    notice: impl Fn(String) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("oriel-gamepads".into())
        .spawn(move || {
            #[cfg(windows)]
            let mut backend = None::<std::sync::Arc<vigem_client::Client>>;
            let mut pads: HashMap<u8, (u32, Box<dyn VirtualPad>)> = HashMap::new();
            let mut failed = false;
            while let Ok(cmd) = cmds.recv() {
                match cmd {
                    Cmd::Remove { pad } => {
                        if pads.remove(&pad).is_some() {
                            tracing::info!(pad, "gamepad unplugged");
                        }
                    }
                    Cmd::State { pad, seq, state } => {
                        if pad >= MAX_PADS {
                            continue;
                        }
                        if let Some((last, p)) = pads.get_mut(&pad) {
                            // Older than what was applied (datagrams reorder).
                            if seq.wrapping_sub(*last) > u32::MAX / 2 {
                                continue;
                            }
                            *last = seq;
                            if let Err(e) = p.update(&state) {
                                tracing::warn!(pad, "gamepad update failed: {e:#}");
                            }
                            continue;
                        }
                        if failed {
                            continue;
                        }
                        #[cfg(windows)]
                        let created = windows::create(&mut backend, pad, rumble.clone());
                        #[cfg(target_os = "linux")]
                        let created = linux::create(pad, rumble.clone());
                        #[cfg(not(any(windows, target_os = "linux")))]
                        let created: Result<Box<dyn VirtualPad>> =
                            Err(anyhow::anyhow!("gamepads are not supported on this host"));
                        match created {
                            Ok(mut p) => {
                                tracing::info!(pad, "gamepad plugged in");
                                let _ = p.update(&state);
                                pads.insert(pad, (seq, p));
                            }
                            Err(e) => {
                                tracing::warn!("cannot create a virtual gamepad: {e:#}");
                                notice(format!("Gamepads unavailable on the host: {e:#}"));
                                failed = true;
                            }
                        }
                    }
                }
            }
        })
        .expect("spawn gamepad thread")
}
