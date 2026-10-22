//! Xbox 360 pads through the ViGEmBus driver.

use std::sync::Arc;

use anyhow::{Context, Result};
use oriel_proto::GamepadState;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

use super::{RumbleFn, VirtualPad};

struct Pad {
    target: Xbox360Wired<Arc<Client>>,
    rumble: Option<std::thread::JoinHandle<()>>,
}

impl VirtualPad for Pad {
    fn update(&mut self, s: &GamepadState) -> Result<()> {
        self.target.update(&XGamepad {
            buttons: XButtons(s.buttons),
            left_trigger: s.left_trigger,
            right_trigger: s.right_trigger,
            thumb_lx: s.lx,
            thumb_ly: s.ly,
            thumb_rx: s.rx,
            thumb_ry: s.ry,
        })?;
        Ok(())
    }
}

impl Drop for Pad {
    fn drop(&mut self) {
        // Unplugging aborts the notification request, ending the thread.
        let _ = self.target.unplug();
        if let Some(t) = self.rumble.take() {
            let _ = t.join();
        }
    }
}

pub(super) fn create(
    client: &mut Option<Arc<Client>>,
    pad: u8,
    rumble: RumbleFn,
) -> Result<Box<dyn VirtualPad>> {
    let client = match client {
        Some(c) => c.clone(),
        None => {
            let c = Arc::new(Client::connect().context(
                "the ViGEmBus driver is not installed \
                 (https://github.com/nefarius/ViGEmBus/releases)",
            )?);
            *client = Some(c.clone());
            c
        }
    };
    let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
    target.plugin().context("plug in a virtual Xbox 360 pad")?;
    target.wait_ready()?;
    let rumble = match target.request_notification() {
        Ok(n) => Some(n.spawn_thread(move |_, n| {
            let scale = |v: u8| v as u16 * 257;
            rumble(pad, scale(n.large_motor), scale(n.small_motor));
        })),
        Err(e) => {
            tracing::warn!("no rumble notifications: {e}");
            None
        }
    };
    Ok(Box::new(Pad { target, rumble }))
}
