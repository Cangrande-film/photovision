//! The monitor profile follows the window between displays (Color Settings › Monitor Profile
//! `auto`).
//!
//! Each frame the shell notes the window's centre. Once the window has stayed put for
//! [`SETTLE`] seconds after a move (and once shortly after launch), it asks the platform
//! ([`crate::Services::detect_monitor_profile`]) for the profile of the display under that point.
//! The query runs in the background; its answer replaces `session.color.monitor_profile` only
//! when the bytes differ, so the canvas cache survives a move within the same display. A manual
//! Monitor Profile in Color Settings still wins (`ColorState::monitor`); the detected bytes are
//! kept up to date underneath it for when the user returns to `auto`.

use crate::PhotocraftApp;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Seconds the window must stay still before the display is queried again.
pub(crate) const SETTLE: f64 = 0.5;
/// Centre movement (points) below which the window counts as not moved.
const MOVE_EPS: f32 = 1.0;

/// Asks the platform for the ICC profile of the display under a screen point (physical
/// pixels). The answer arrives on the receiver: `Some(bytes)` is the profile, `None` means
/// unknown (the current profile is kept).
pub type DetectMonitorFn = Box<dyn Fn([f32; 2]) -> Receiver<Option<Vec<u8>>>>;

/// Debounce state: where the window was, when it last moved, and the query in flight.
#[derive(Default)]
pub(crate) struct MonitorFollow {
    last: Option<[f32; 2]>,
    moved_at: Option<f64>,
    queried: Option<[f32; 2]>,
    pending: Option<Receiver<Option<Vec<u8>>>>,
}

impl MonitorFollow {
    /// Records the window centre at time `now` (seconds); returns the point to query when the
    /// window has settled at a position not queried yet and no query is in flight.
    pub(crate) fn observe(&mut self, centre: [f32; 2], now: f64) -> Option<[f32; 2]> {
        let moved = self.last.is_none_or(|l| (l[0] - centre[0]).abs() > MOVE_EPS || (l[1] - centre[1]).abs() > MOVE_EPS);
        if moved {
            self.last = Some(centre);
            self.moved_at = Some(now);
            return None;
        }
        let since = self.moved_at?;
        if now - since < SETTLE || self.pending.is_some() {
            return None;
        }
        self.moved_at = None;
        let same = self.queried.is_some_and(|q| (q[0] - centre[0]).abs() <= MOVE_EPS && (q[1] - centre[1]).abs() <= MOVE_EPS);
        if same {
            return None;
        }
        self.queried = Some(centre);
        Some(centre)
    }

    /// Whether frames are needed to finish the debounce or receive a result.
    pub(crate) fn waiting(&self) -> bool {
        self.moved_at.is_some() || self.pending.is_some()
    }

    /// The finished query's profile, if one arrived this frame (`Some(None)`: unknown).
    pub(crate) fn poll(&mut self) -> Option<Option<Vec<u8>>> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(r) => {
                self.pending = None;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                Some(None)
            }
        }
    }
}

/// Applies a detected profile: replaces the session's monitor bytes only when they changed.
/// Returns whether anything changed.
pub(crate) fn apply(app: &mut PhotocraftApp, detected: Option<Vec<u8>>) -> bool {
    let Some(bytes) = detected else { return false };
    if app.session.color.monitor_profile.as_deref().is_some_and(|cur| *cur == bytes) {
        return false;
    }
    app.session.color.monitor_profile = Some(Arc::new(bytes));
    true
}

/// Per-frame step: watch the window, start a query when it settles, apply results.
pub(crate) fn tick(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if app.services.detect_monitor_profile.is_none() {
        return;
    }
    if let Some(r) = app.monitor_follow.poll()
        && apply(app, r)
    {
        log::info!("monitor profile: {}", app.session.color.monitor().description);
        ctx.request_repaint();
    }
    let Some(rect) = ctx.input(|i| i.viewport().outer_rect) else { return };
    let now = ctx.input(|i| i.time);
    // `outer_rect` is in points (UI zoom × the display's scale); the platform wants pixels.
    let ppp = ctx.pixels_per_point();
    let c = rect.center();
    if let Some(p) = app.monitor_follow.observe([c.x * ppp, c.y * ppp], now)
        && let Some(detect) = &app.services.detect_monitor_profile
    {
        app.monitor_follow.pending = Some(detect(p));
    }
    if app.monitor_follow.waiting() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_once_after_settling() {
        let mut f = MonitorFollow::default();
        assert_eq!(f.observe([10.0, 10.0], 0.0), None, "first sighting starts the debounce");
        assert_eq!(f.observe([10.0, 10.0], 0.2), None);
        assert_eq!(f.observe([10.0, 10.0], 0.6), Some([10.0, 10.0]));
        assert!(!f.waiting());
        assert_eq!(f.observe([10.0, 10.0], 5.0), None, "no repeat while still");
    }

    #[test]
    fn moving_restarts_the_debounce_and_same_spot_is_not_requeried() {
        let mut f = MonitorFollow::default();
        f.observe([0.0, 0.0], 0.0);
        assert!(f.observe([0.0, 0.0], 1.0).is_some());
        // Dragging: every move restarts the timer.
        assert_eq!(f.observe([100.0, 0.0], 1.1), None);
        assert_eq!(f.observe([200.0, 0.0], 1.4), None);
        assert_eq!(f.observe([200.0, 0.0], 1.8), None, "only 0.4 s still");
        assert_eq!(f.observe([200.0, 0.0], 1.95), Some([200.0, 0.0]));
        // Moved away and back to the queried spot: nothing to do.
        f.observe([300.0, 0.0], 2.0);
        f.observe([200.5, 0.0], 2.1);
        assert_eq!(f.observe([200.5, 0.0], 3.0), None);
        assert!(!f.waiting());
    }

    #[test]
    fn waits_for_the_query_in_flight() {
        let mut f = MonitorFollow::default();
        let (tx, rx) = std::sync::mpsc::channel();
        f.pending = Some(rx);
        f.observe([5.0, 5.0], 0.0);
        assert_eq!(f.observe([5.0, 5.0], 1.0), None, "a query is in flight");
        assert!(f.waiting());
        assert_eq!(f.poll(), None);
        tx.send(Some(vec![1, 2, 3])).unwrap();
        assert_eq!(f.poll(), Some(Some(vec![1, 2, 3])));
        assert_eq!(f.observe([5.0, 5.0], 1.1), Some([5.0, 5.0]), "deferred query runs next");
        // A worker that died reads as unknown.
        let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
        drop(tx);
        f.pending = Some(rx);
        assert_eq!(f.poll(), Some(None));
        assert!(f.pending.is_none());
    }

    #[test]
    fn apply_keeps_the_arc_when_bytes_match() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        assert!(!apply(&mut app, None));
        assert!(app.session.color.monitor_profile.is_none());
        assert!(apply(&mut app, Some(vec![7; 4])));
        let first = app.session.color.monitor_profile.clone().unwrap();
        assert!(!apply(&mut app, Some(vec![7; 4])));
        assert!(Arc::ptr_eq(&first, app.session.color.monitor_profile.as_ref().unwrap()));
        assert!(!apply(&mut app, None), "unknown keeps the current profile");
        assert!(apply(&mut app, Some(vec![8; 4])));
    }
}
