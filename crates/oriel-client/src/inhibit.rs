//! Wayland: ask the compositor to pass its own shortcuts (Super, Alt+Tab,
//! ...) to us while the remote desktop has the keyboard. GNOME asks the user
//! to allow this the first time.

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use winit::{
    raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle},
    window::Window,
};

#[derive(Default)]
struct State {
    active: bool,
}

pub struct ShortcutInhibitor {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    seat: WlSeat,
    surface: WlSurface,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl ShortcutInhibitor {
    /// `None` when not on Wayland or the compositor lacks the protocol.
    pub fn new(window: &Window) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else {
            return None;
        };
        let RawWindowHandle::Wayland(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: winit keeps the display alive for the window's lifetime,
        // and we are dropped before the window.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let manager =
            match globals.bind::<ZwpKeyboardShortcutsInhibitManagerV1, _, _>(&qh, 1..=1, ()) {
                Ok(m) => m,
                Err(e) => {
                    tracing::info!("compositor cannot pass shortcuts through: {e}");
                    return None;
                }
            };
        let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=1, ()).ok()?;
        let surface_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .ok()?;
        let surface = WlSurface::from_id(&conn, surface_id).ok()?;
        Some(Self {
            conn,
            queue,
            state: State::default(),
            manager,
            seat,
            surface,
            inhibitor: None,
        })
    }

    pub fn set(&mut self, inhibit: bool) {
        match (inhibit, &self.inhibitor) {
            (true, None) => {
                let qh = self.queue.handle();
                self.inhibitor =
                    Some(
                        self.manager
                            .inhibit_shortcuts(&self.surface, &self.seat, &qh, ()),
                    );
            }
            (false, Some(_)) => {
                if let Some(i) = self.inhibitor.take() {
                    i.destroy();
                }
            }
            _ => return,
        }
        let _ = self.conn.flush();
    }

    /// Whether the compositor currently passes shortcuts to us.
    pub fn active(&mut self) -> bool {
        let _ = self.queue.dispatch_pending(&mut self.state);
        self.inhibitor.is_some() && self.state.active
    }
}

impl Drop for ShortcutInhibitor {
    fn drop(&mut self) {
        self.set(false);
        self.manager.destroy();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitManagerV1,
        _: <ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => state.active = true,
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => state.active = false,
            _ => {}
        }
    }
}
