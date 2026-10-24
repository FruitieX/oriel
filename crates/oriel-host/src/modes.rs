//! Display modes asked for by sessions. With several clients connected,
//! the latest request for a display wins; when its session lets go, the
//! one before comes back, and the display's own mode once nobody wants
//! one. Only the mode in force is set again when Windows drops it.

use anyhow::Result;
use parking_lot::Mutex;

use crate::capture::{self, ModeGuard, SourceKind};

struct Claim {
    id: u64,
    source: SourceKind,
    display: Option<u32>,
    size: (u32, u32),
}

struct Modes {
    next_id: u64,
    /// Oldest first.
    claims: Vec<Claim>,
    /// The mode set on each display, by the latest claim for it.
    applied: Vec<(Option<u32>, ModeGuard)>,
}

static MODES: Mutex<Modes> = Mutex::new(Modes {
    next_id: 0,
    claims: Vec::new(),
    applied: Vec::new(),
});

impl Modes {
    /// Set `guard` as `display`'s mode, replacing (not restoring) the one
    /// before.
    fn apply(&mut self, display: Option<u32>, guard: ModeGuard) {
        if let Some(i) = self.applied.iter().position(|(d, _)| *d == display) {
            let (_, old) = self.applied.swap_remove(i);
            old.keep();
        }
        self.applied.push((display, guard));
    }

    fn latest(&self, display: Option<u32>) -> Option<&Claim> {
        self.claims.iter().rev().find(|c| c.display == display)
    }
}

/// A session's wish for a display mode, given up when dropped.
pub struct ModeClaim {
    id: u64,
    display: Option<u32>,
    size: (u32, u32),
}

/// Switch `display` to `size`, or the closest size it supports.
pub fn claim(source: SourceKind, display: Option<u32>, size: (u32, u32)) -> Result<ModeClaim> {
    let mut modes = MODES.lock();
    let guard = capture::set_display_mode(source, display, size)?;
    let set = guard.size();
    modes.apply(display, guard);
    let id = modes.next_id;
    modes.next_id += 1;
    modes.claims.push(Claim {
        id,
        source,
        display,
        size,
    });
    Ok(ModeClaim {
        id,
        display,
        size: set,
    })
}

impl ModeClaim {
    /// The size set (the closest the display supports).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Set the mode again if Windows dropped it, when this claim is the
    /// one in force. True when it did.
    pub fn reapply(&self) -> bool {
        let modes = MODES.lock();
        if modes.latest(self.display).map(|c| c.id) != Some(self.id) {
            return false;
        }
        modes
            .applied
            .iter()
            .find(|(d, _)| *d == self.display)
            .is_some_and(|(_, g)| g.reapply())
    }
}

impl Drop for ModeClaim {
    fn drop(&mut self) {
        let mut modes = MODES.lock();
        let in_force = modes.latest(self.display).map(|c| c.id) == Some(self.id);
        modes.claims.retain(|c| c.id != self.id);
        if !in_force {
            return;
        }
        let previous = modes
            .latest(self.display)
            .map(|c| (c.source, c.size));
        match previous.map(|(source, size)| capture::set_display_mode(source, self.display, size)) {
            Some(Ok(guard)) => {
                tracing::info!(size = ?guard.size(), "back to the previous client's display mode");
                modes.apply(self.display, guard);
            }
            other => {
                if let Some(Err(e)) = other {
                    tracing::warn!("cannot set the previous client's display mode: {e:#}");
                }
                // Dropping the guard restores the display's own mode.
                modes.applied.retain(|(d, _)| *d != self.display);
            }
        }
    }
}
