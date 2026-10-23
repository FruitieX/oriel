//! Touch screens: gestures become mouse input, and two fingers zoom and pan
//! the picture locally (the host never sees the zoom).
//!
//! - Tap: left click where tapped. Drag: left button held while moving.
//! - Long press: right click. Two-finger tap: right click.
//! - Two fingers: pinch to zoom, move to pan; scrolls instead while the
//!   whole picture is in view. Three fingers: scroll.

use std::time::{Duration, Instant};

use oriel_proto::MouseButton;

use crate::render::Viewport;

const LONG_PRESS: Duration = Duration::from_millis(500);
/// A two-finger tap is quicker than this.
const MULTI_TAP: Duration = Duration::from_millis(300);
pub const MAX_ZOOM: f32 = 8.0;

/// What a gesture asks for; positions in window pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    MoveTo(f32, f32),
    Button(MouseButton, bool),
    /// Scroll by this many pixels of finger motion (positive: fingers moved
    /// down, i.e. towards the top of the content).
    Scroll(f32, f32),
    /// Two fingers moved: centre now, how far it moved, and the change in
    /// their distance (1: none).
    Pinch {
        mid: (f32, f32),
        moved: (f32, f32),
        scale: f32,
    },
}

#[derive(Debug, Clone, Copy)]
struct Finger {
    id: u64,
    start: (f32, f32),
    pos: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    /// One finger down, not yet a tap, drag or long press.
    Pending {
        since: Instant,
    },
    Dragging,
    /// Two or more fingers. `moved` once they went past the slop.
    Multi {
        since: Instant,
        most: usize,
        moved: bool,
    },
    /// Handled (a long press); ignore until every finger is up.
    Done,
}

pub struct Gestures {
    fingers: Vec<Finger>,
    mode: Mode,
    /// Movement below this (pixels) is still a tap.
    slop: f32,
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn mid(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

impl Gestures {
    pub fn new(slop: f32) -> Self {
        Self {
            fingers: Vec::new(),
            mode: Mode::Idle,
            slop,
        }
    }

    pub fn active(&self) -> bool {
        !self.fingers.is_empty()
    }

    /// When `tick` has something to do (a pending long press).
    pub fn deadline(&self) -> Option<Instant> {
        match self.mode {
            Mode::Pending { since } => Some(since + LONG_PRESS),
            _ => None,
        }
    }

    pub fn down(&mut self, id: u64, pos: (f32, f32), now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        self.fingers.retain(|f| f.id != id);
        self.fingers.push(Finger {
            id,
            start: pos,
            pos,
        });
        match (self.mode, self.fingers.len()) {
            (Mode::Idle, 1) => self.mode = Mode::Pending { since: now },
            (Mode::Pending { .. } | Mode::Dragging, _) => {
                if self.mode == Mode::Dragging {
                    out.push(Action::Button(MouseButton::Left, false));
                }
                self.mode = Mode::Multi {
                    since: now,
                    most: self.fingers.len(),
                    moved: self.mode == Mode::Dragging,
                };
                for f in &mut self.fingers {
                    f.start = f.pos;
                }
            }
            (Mode::Multi { since, moved, most }, n) => {
                self.mode = Mode::Multi {
                    since,
                    moved,
                    most: most.max(n),
                }
            }
            _ => {}
        }
        out
    }

    pub fn moved(&mut self, id: u64, pos: (f32, f32)) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(i) = self.fingers.iter().position(|f| f.id == id) else {
            return out;
        };
        let before: Vec<(f32, f32)> = self.fingers.iter().map(|f| f.pos).collect();
        self.fingers[i].pos = pos;
        match self.mode {
            Mode::Pending { .. } => {
                let f = self.fingers[0];
                if dist(f.start, f.pos) > self.slop {
                    out.push(Action::MoveTo(f.start.0, f.start.1));
                    out.push(Action::Button(MouseButton::Left, true));
                    out.push(Action::MoveTo(pos.0, pos.1));
                    self.mode = Mode::Dragging;
                }
            }
            Mode::Dragging => out.push(Action::MoveTo(pos.0, pos.1)),
            Mode::Multi { since, most, moved } => {
                let gone = self
                    .fingers
                    .iter()
                    .any(|f| dist(f.start, f.pos) > self.slop);
                self.mode = Mode::Multi {
                    since,
                    most,
                    moved: moved || gone,
                };
                if !(moved || gone) || self.fingers.len() < 2 {
                    return out;
                }
                let (a0, b0) = (before[0], before[1]);
                let (a1, b1) = (self.fingers[0].pos, self.fingers[1].pos);
                let (m0, m1) = (mid(a0, b0), mid(a1, b1));
                let delta = (m1.0 - m0.0, m1.1 - m0.1);
                if self.fingers.len() >= 3 {
                    out.push(Action::Scroll(delta.0, delta.1));
                } else {
                    let scale = dist(a1, b1) / dist(a0, b0).max(1.0);
                    out.push(Action::Pinch {
                        mid: m1,
                        moved: delta,
                        scale,
                    });
                }
            }
            Mode::Idle | Mode::Done => {}
        }
        out
    }

    pub fn up(&mut self, id: u64, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(i) = self.fingers.iter().position(|f| f.id == id) else {
            return out;
        };
        let finger = self.fingers.remove(i);
        match self.mode {
            Mode::Pending { .. } => {
                out.push(Action::MoveTo(finger.start.0, finger.start.1));
                out.push(Action::Button(MouseButton::Left, true));
                out.push(Action::Button(MouseButton::Left, false));
            }
            Mode::Dragging => {
                out.push(Action::MoveTo(finger.pos.0, finger.pos.1));
                out.push(Action::Button(MouseButton::Left, false));
            }
            // A quick two-finger tap.
            Mode::Multi { since, most, moved }
                if self.fingers.is_empty()
                    && !moved
                    && most == 2
                    && now.duration_since(since) < MULTI_TAP =>
            {
                out.push(Action::Button(MouseButton::Right, true));
                out.push(Action::Button(MouseButton::Right, false));
            }
            _ => {}
        }
        if self.fingers.is_empty() {
            self.mode = Mode::Idle;
        } else if !matches!(self.mode, Mode::Multi { .. }) {
            self.mode = Mode::Done;
        }
        out
    }

    /// Cancel everything (the system took the touches).
    pub fn cancel(&mut self) -> Vec<Action> {
        let out = if self.mode == Mode::Dragging {
            vec![Action::Button(MouseButton::Left, false)]
        } else {
            Vec::new()
        };
        self.fingers.clear();
        self.mode = Mode::Idle;
        out
    }

    /// A finger held still long enough is a right click.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        if let Mode::Pending { since } = self.mode
            && now.duration_since(since) >= LONG_PRESS
            && let Some(f) = self.fingers.first()
        {
            self.mode = Mode::Done;
            return vec![
                Action::MoveTo(f.pos.0, f.pos.1),
                Action::Button(MouseButton::Right, true),
                Action::Button(MouseButton::Right, false),
            ];
        }
        Vec::new()
    }
}

/// The local zoom and pan of the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// 1: the whole picture fits the window.
    pub zoom: f32,
    /// The picture's centre, offset from the window's centre (pixels).
    pub offset: (f32, f32),
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            offset: (0.0, 0.0),
        }
    }
}

impl View {
    /// Where the picture goes: `fit` (the letterboxed picture) zoomed and
    /// moved, kept covering the window where it is larger than it.
    pub fn apply(&self, fit: Viewport, window: (f32, f32)) -> Viewport {
        let (w, h) = (fit.w * self.zoom, fit.h * self.zoom);
        let place = |size: f32, win: f32, offset: f32| {
            let x = (win - size) / 2.0 + offset;
            if size <= win {
                (win - size) / 2.0
            } else {
                x.clamp(win - size, 0.0)
            }
        };
        Viewport {
            x: place(w, window.0, self.offset.0),
            y: place(h, window.1, self.offset.1),
            w,
            h,
        }
    }

    /// Zoom about `mid` by `scale` and move by `moved`. Returns the part of
    /// `moved` the picture could not follow (it was already at its edge).
    pub fn pinch(
        &mut self,
        fit: Viewport,
        window: (f32, f32),
        mid: (f32, f32),
        moved: (f32, f32),
        scale: f32,
    ) -> (f32, f32) {
        let before = self.apply(fit, window);
        let zoom = (self.zoom * scale).clamp(1.0, MAX_ZOOM);
        let k = zoom / self.zoom;
        // Keep the point under the fingers under them.
        let x = mid.0 - (mid.0 - before.x) * k + moved.0;
        let y = mid.1 - (mid.1 - before.y) * k + moved.1;
        let (w, h) = (fit.w * zoom, fit.h * zoom);
        self.zoom = zoom;
        self.offset = (x + w / 2.0 - window.0 / 2.0, y + h / 2.0 - window.1 / 2.0);
        let after = self.apply(fit, window);
        // Store the clamped position, so moving back works at once.
        self.offset = (
            after.x + w / 2.0 - window.0 / 2.0,
            after.y + h / 2.0 - window.1 / 2.0,
        );
        (x - after.x, y - after.y)
    }

    pub fn zoomed(&self) -> bool {
        self.zoom > 1.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn tap_clicks_where_tapped() {
        let t0 = Instant::now();
        let mut g = Gestures::new(10.0);
        assert!(g.down(1, (100.0, 50.0), t0).is_empty());
        assert!(g.moved(1, (103.0, 52.0)).is_empty());
        assert_eq!(
            g.up(1, at(t0, 100)),
            vec![
                Action::MoveTo(100.0, 50.0),
                Action::Button(MouseButton::Left, true),
                Action::Button(MouseButton::Left, false),
            ]
        );
        assert!(!g.active());
    }

    #[test]
    fn drag_holds_the_button() {
        let t0 = Instant::now();
        let mut g = Gestures::new(10.0);
        g.down(1, (0.0, 0.0), t0);
        assert_eq!(
            g.moved(1, (30.0, 0.0)),
            vec![
                Action::MoveTo(0.0, 0.0),
                Action::Button(MouseButton::Left, true),
                Action::MoveTo(30.0, 0.0),
            ]
        );
        assert_eq!(g.moved(1, (40.0, 5.0)), vec![Action::MoveTo(40.0, 5.0)]);
        assert_eq!(
            g.up(1, at(t0, 900)),
            vec![
                Action::MoveTo(40.0, 5.0),
                Action::Button(MouseButton::Left, false)
            ]
        );
    }

    #[test]
    fn long_press_right_clicks_once() {
        let t0 = Instant::now();
        let mut g = Gestures::new(10.0);
        g.down(1, (5.0, 5.0), t0);
        assert!(g.tick(at(t0, 200)).is_empty());
        assert_eq!(g.tick(at(t0, 600)).len(), 3);
        assert!(g.tick(at(t0, 700)).is_empty());
        assert!(g.moved(1, (50.0, 50.0)).is_empty());
        assert!(g.up(1, at(t0, 800)).is_empty());
    }

    #[test]
    fn two_finger_tap_right_clicks_and_pinch_does_not_click() {
        let t0 = Instant::now();
        let mut g = Gestures::new(10.0);
        g.down(1, (0.0, 0.0), t0);
        g.down(2, (100.0, 0.0), at(t0, 20));
        g.up(1, at(t0, 120));
        assert_eq!(
            g.up(2, at(t0, 130)),
            vec![
                Action::Button(MouseButton::Right, true),
                Action::Button(MouseButton::Right, false)
            ]
        );

        g.down(1, (0.0, 0.0), t0);
        g.down(2, (100.0, 0.0), t0);
        let a = g.moved(2, (200.0, 0.0));
        assert!(
            matches!(a[..], [Action::Pinch { scale, .. }] if (scale - 2.0).abs() < 0.01),
            "{a:?}"
        );
        g.up(1, at(t0, 100));
        assert!(g.up(2, at(t0, 110)).is_empty());
        // The finger left behind does not start a drag.
        g.down(1, (0.0, 0.0), at(t0, 200));
        g.down(2, (10.0, 0.0), at(t0, 210));
        g.up(2, at(t0, 600));
        assert!(g.moved(1, (80.0, 80.0)).is_empty());
    }

    #[test]
    fn three_fingers_scroll() {
        let t0 = Instant::now();
        let mut g = Gestures::new(10.0);
        g.down(1, (0.0, 0.0), t0);
        g.down(2, (50.0, 0.0), t0);
        g.down(3, (100.0, 0.0), t0);
        g.moved(1, (0.0, 20.0));
        let a = g.moved(2, (50.0, 20.0));
        assert!(
            matches!(a[..], [Action::Scroll(_, dy)] if dy > 0.0),
            "{a:?}"
        );
    }

    #[test]
    fn view_zooms_about_the_fingers_and_stays_on_screen() {
        let fit = Viewport {
            x: 0.0,
            y: 300.0,
            w: 1000.0,
            h: 500.0,
        };
        let window = (1000.0, 1100.0);
        let mut v = View::default();
        assert_eq!(v.apply(fit, window), fit);
        // Zoom 2x about a point on the picture: that point stays put.
        v.pinch(fit, window, (250.0, 425.0), (0.0, 0.0), 2.0);
        let r = v.apply(fit, window);
        assert_eq!((r.w, r.h), (2000.0, 1000.0));
        assert!((r.x - -250.0).abs() < 0.01, "{r:?}");
        // Taller than before but not than the window: centred vertically.
        assert!((r.y - 50.0).abs() < 0.01, "{r:?}");
        // Panning past the edge stops there and reports the rest.
        let rest = v.pinch(fit, window, (500.0, 500.0), (1000.0, 0.0), 1.0);
        assert_eq!(v.apply(fit, window).x, 0.0);
        assert!((rest.0 - 750.0).abs() < 0.01, "{rest:?}");
        // Zooming out never goes below fitting.
        v.pinch(fit, window, (500.0, 500.0), (0.0, 0.0), 0.1);
        assert_eq!(v.apply(fit, window), fit);
        assert!(!v.zoomed());
    }
}
