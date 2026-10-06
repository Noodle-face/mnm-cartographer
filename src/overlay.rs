//! Overlay mode: the map on top of the game, borderless and see-through.
//!
//! An overlay has two states, swapped by one global hotkey that works while
//! the game has focus:
//!
//! - **using** the map: it takes the mouse, at its normal opacity;
//! - **playing**: clicks pass through to the game, and the map shows at a
//!   second, usually lower, opacity -- zero hides it outright.
//!
//! Click-through needs a way back that is not a click, and a hidden window
//! needs a way back that is not the window; the hotkey is both. It is only
//! registered while the overlay is on, so the key combination is not taken
//! from other programs the rest of the time.
//!
//! Always-on-top and the hotkey are up to the platform. Windows does both.
//! On Linux the hotkey works through X11, which includes a game run through
//! Proton under XWayland; a native Wayland window cannot be kept on top by
//! the app itself, so there it takes a window rule in the compositor.

use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::str::FromStr;
use std::sync::mpsc::{channel, Receiver};

/// Offered in the overlay bar. Combinations a game is unlikely to bind.
pub const HOTKEYS: &[&str] = &["Ctrl+Shift+M", "Ctrl+Shift+O", "Ctrl+Alt+M", "F9", "F10", "F11"];
pub const DEFAULT_HOTKEY: &str = "Ctrl+Shift+M";
/// Opacity while using the map, and while playing, unless set otherwise.
pub const DEFAULT_OPACITY: f32 = 0.9;
pub const DEFAULT_PASSIVE: f32 = 0.0;

pub struct Hotkey {
    manager: Option<GlobalHotKeyManager>,
    current: Option<(String, HotKey)>,
    rx: Receiver<()>,
    /// Why the hotkey is not working, if it is not.
    pub error: String,
}

impl Hotkey {
    /// Must run on the UI thread: on Windows a hotkey belongs to the thread
    /// that registers it, and that thread must be pumping messages.
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        // Called from the platform's own thread; waking the UI is what lets a
        // hidden overlay notice the key at all.
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                let _ = tx.send(());
                ctx.request_repaint();
            }
        }));
        let (manager, error) = match GlobalHotKeyManager::new() {
            Ok(m) => (Some(m), String::new()),
            Err(e) => (None, format!("no global hotkeys here: {e}")),
        };
        Self { manager, current: None, rx, error }
    }

    /// Listen for `spec` (e.g. "Ctrl+Shift+M"), or for nothing with `None`.
    pub fn set(&mut self, spec: Option<&str>) {
        let Some(m) = &self.manager else { return };
        if self.current.as_ref().map(|c| c.0.as_str()) == spec { return }
        if let Some((_, k)) = self.current.take() {
            let _ = m.unregister(k);
        }
        self.error.clear();
        let Some(spec) = spec else { return };
        match HotKey::from_str(spec) {
            Ok(k) => match m.register(k) {
                Ok(()) => self.current = Some((spec.to_string(), k)),
                Err(e) => self.error = format!("{spec} is unavailable: {e}"),
            },
            Err(e) => self.error = format!("{spec}: {e}"),
        }
    }

    /// Whether the key was pressed since the last call.
    pub fn pressed(&self) -> bool {
        self.rx.try_iter().count() % 2 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_hotkeys_parse() {
        for h in HOTKEYS {
            assert!(HotKey::from_str(h).is_ok(), "{h}");
        }
        assert!(HOTKEYS.contains(&DEFAULT_HOTKEY));
    }
}
