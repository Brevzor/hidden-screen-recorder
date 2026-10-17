use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::debug_log;
use eframe::egui::{Key, Modifiers};
use serde::{Deserialize, Serialize};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};

const KEYS: &[(Key, u16, &str)] = &[
    (Key::A, 0x41, "A"),
    (Key::B, 0x42, "B"),
    (Key::C, 0x43, "C"),
    (Key::D, 0x44, "D"),
    (Key::E, 0x45, "E"),
    (Key::F, 0x46, "F"),
    (Key::G, 0x47, "G"),
    (Key::H, 0x48, "H"),
    (Key::I, 0x49, "I"),
    (Key::J, 0x4A, "J"),
    (Key::K, 0x4B, "K"),
    (Key::L, 0x4C, "L"),
    (Key::M, 0x4D, "M"),
    (Key::N, 0x4E, "N"),
    (Key::O, 0x4F, "O"),
    (Key::P, 0x50, "P"),
    (Key::Q, 0x51, "Q"),
    (Key::R, 0x52, "R"),
    (Key::S, 0x53, "S"),
    (Key::T, 0x54, "T"),
    (Key::U, 0x55, "U"),
    (Key::V, 0x56, "V"),
    (Key::W, 0x57, "W"),
    (Key::X, 0x58, "X"),
    (Key::Y, 0x59, "Y"),
    (Key::Z, 0x5A, "Z"),
    (Key::Num0, 0x30, "0"),
    (Key::Num1, 0x31, "1"),
    (Key::Num2, 0x32, "2"),
    (Key::Num3, 0x33, "3"),
    (Key::Num4, 0x34, "4"),
    (Key::Num5, 0x35, "5"),
    (Key::Num6, 0x36, "6"),
    (Key::Num7, 0x37, "7"),
    (Key::Num8, 0x38, "8"),
    (Key::Num9, 0x39, "9"),
    (Key::F1, 0x70, "F1"),
    (Key::F2, 0x71, "F2"),
    (Key::F3, 0x72, "F3"),
    (Key::F4, 0x73, "F4"),
    (Key::F5, 0x74, "F5"),
    (Key::F6, 0x75, "F6"),
    (Key::F7, 0x76, "F7"),
    (Key::F8, 0x77, "F8"),
    (Key::F9, 0x78, "F9"),
    (Key::F10, 0x79, "F10"),
    (Key::F11, 0x7A, "F11"),
    (Key::F12, 0x7B, "F12"),
    (Key::F13, 0x7C, "F13"),
    (Key::F14, 0x7D, "F14"),
    (Key::F15, 0x7E, "F15"),
    (Key::F16, 0x7F, "F16"),
    (Key::F17, 0x80, "F17"),
    (Key::F18, 0x81, "F18"),
    (Key::F19, 0x82, "F19"),
    (Key::F20, 0x83, "F20"),
    (Key::F21, 0x84, "F21"),
    (Key::F22, 0x85, "F22"),
    (Key::F23, 0x86, "F23"),
    (Key::F24, 0x87, "F24"),
    (Key::Space, 0x20, "Space"),
    (Key::Enter, 0x0D, "Enter"),
    (Key::Tab, 0x09, "Tab"),
    (Key::Backspace, 0x08, "Backspace"),
    (Key::Insert, 0x2D, "Insert"),
    (Key::Delete, 0x2E, "Delete"),
    (Key::Home, 0x24, "Home"),
    (Key::End, 0x23, "End"),
    (Key::PageUp, 0x21, "PageUp"),
    (Key::PageDown, 0x22, "PageDown"),
    (Key::ArrowLeft, 0x25, "Left"),
    (Key::ArrowUp, 0x26, "Up"),
    (Key::ArrowRight, 0x27, "Right"),
    (Key::ArrowDown, 0x28, "Down"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub key: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Shortcut {
    pub fn from_input(key: Key, modifiers: Modifiers) -> Option<Self> {
        let key = KEYS.iter().find(|entry| entry.0 == key)?.1;
        let shortcut = Self {
            key,
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
        };
        shortcut.is_valid().then_some(shortcut)
    }

    fn is_valid(self) -> bool {
        KEYS.iter().any(|entry| entry.1 == self.key)
            && (self.ctrl || self.alt || self.shift || (0x70..=0x87).contains(&self.key))
            && !(self.alt && self.key == 0x73) // Alt+F4 closes the application.
    }

    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.push(
            KEYS.iter()
                .find(|entry| entry.1 == self.key)
                .map_or("?", |entry| entry.2),
        );
        parts.join("+")
    }

    fn matches(self, key: bool, ctrl: bool, alt: bool, shift: bool) -> bool {
        key && self.ctrl == ctrl && self.alt == alt && self.shift == shift
    }

    fn is_down(self, ctrl: bool, alt: bool, shift: bool) -> bool {
        self.matches(key_down(i32::from(self.key)), ctrl, alt, shift)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub start: Shortcut,
    pub stop: Shortcut,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            start: Shortcut {
                key: 0x78,
                ctrl: true,
                alt: false,
                shift: false,
            },
            stop: Shortcut {
                key: 0x79,
                ctrl: true,
                alt: false,
                shift: false,
            },
        }
    }
}

impl Shortcuts {
    pub fn is_valid(self) -> bool {
        self.start.is_valid() && self.stop.is_valid() && self.start != self.stop
    }
}

#[derive(Clone, Copy)]
struct ControlState {
    shortcuts: Shortcuts,
    suspended: bool,
    revision: u64,
}

pub struct HotkeyController {
    state: Mutex<ControlState>,
    stop: Mutex<Arc<AtomicBool>>,
    shutdown: AtomicBool,
}

impl HotkeyController {
    pub fn new(shortcuts: Shortcuts) -> Self {
        Self {
            stop: Mutex::new(Arc::new(AtomicBool::new(false))),
            shutdown: AtomicBool::new(false),
            state: Mutex::new(ControlState {
                shortcuts,
                suspended: false,
                revision: 0,
            }),
        }
    }

    pub fn set_stop_signal(&self, stop: Arc<AtomicBool>) {
        *self.stop.lock().unwrap_or_else(|error| error.into_inner()) = stop;
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn suspend(&self, suspended: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.suspended = suspended;
        state.revision = state.revision.wrapping_add(1);
    }

    pub fn update(&self, shortcuts: Shortcuts) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.shortcuts = shortcuts;
        state.revision = state.revision.wrapping_add(1);
    }

    fn snapshot(&self) -> ControlState {
        *self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Start,
    Stop,
    Availability { start: bool, stop: bool },
}

#[derive(Default)]
struct KeyEdges {
    revision: Option<u64>,
    armed: bool,
    start_down: bool,
    stop_down: bool,
}

impl KeyEdges {
    fn poll(&mut self, state: ControlState, start: bool, stop: bool) -> (bool, bool) {
        if self.revision != Some(state.revision) || state.suspended {
            self.revision = Some(state.revision);
            self.armed = false;
        }
        let events = if !self.armed || state.suspended {
            if !state.suspended && !start && !stop {
                self.armed = true;
            }
            (false, false)
        } else {
            (start && !self.start_down, stop && !self.stop_down)
        };
        self.start_down = start;
        self.stop_down = stop;
        events
    }
}

pub fn spawn(
    sender: Sender<HotkeyEvent>,
    controller: Arc<HotkeyController>,
    wake: impl Fn() + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("global-hotkeys".into())
        .spawn(move || {
            let _ = sender.send(HotkeyEvent::Availability {
                start: true,
                stop: true,
            });
            debug_log::write("hotkey polling thread started (8 ms)");
            wake();
            let mut edges = KeyEdges::default();
            while !controller.shutdown.load(Ordering::Acquire) {
                let state = controller.snapshot();
                let ctrl = key_down(VK_CONTROL.0 as i32);
                let alt = key_down(VK_MENU.0 as i32);
                let shift = key_down(VK_SHIFT.0 as i32);
                let (start, stop) = edges.poll(
                    state,
                    state.shortcuts.start.is_down(ctrl, alt, shift),
                    state.shortcuts.stop.is_down(ctrl, alt, shift),
                );
                if stop {
                    signal_stop(
                        &controller
                            .stop
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                    );
                    if sender.send(HotkeyEvent::Stop).is_err() {
                        break;
                    }
                    debug_log::write("stop shortcut detected");
                    wake();
                } else if start {
                    if sender.send(HotkeyEvent::Start).is_err() {
                        break;
                    }
                    debug_log::write("start shortcut detected");
                    wake();
                }
                thread::sleep(Duration::from_millis(8));
            }
        })
}

/// egui merges keypad and top-row digits. Accept only the actual Win32 key
/// that our global polling will observe, also checking layout differences.
pub fn input_matches_polling(shortcut: Shortcut) -> bool {
    input_matches(shortcut, key_down)
}

fn input_matches(shortcut: Shortcut, is_down: impl Fn(i32) -> bool) -> bool {
    is_down(i32::from(shortcut.key))
        && !is_down(0x5B)
        && !is_down(0x5C)
        && !(0x60..=0x6F).any(is_down)
}

fn key_down(virtual_key: i32) -> bool {
    (unsafe { GetAsyncKeyState(virtual_key) } as u16 & 0x8000) != 0
}

fn signal_stop(stop_signal: &AtomicBool) {
    // Stopping does not depend on UI repainting or another captured frame.
    stop_signal.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypad_input_cannot_be_saved_as_a_top_row_digit() {
        let shortcut = Shortcut::from_input(Key::Num0, Modifiers::CTRL).unwrap();
        assert!(input_matches(shortcut, |key| key == 0x30));
        assert!(!input_matches(shortcut, |key| key == 0x60));
        assert!(!input_matches(shortcut, |key| key == 0x30 || key == 0x60));
        assert!(!input_matches(shortcut, |key| key == 0x30 || key == 0x5B));
    }

    #[test]
    fn hotkey_worker_stops_while_idle() {
        let controller = Arc::new(HotkeyController::new(Shortcuts::default()));
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = spawn(sender, controller.clone(), || {}).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            HotkeyEvent::Availability { stop: true, .. }
        ));
        controller.shutdown();
        handle.join().unwrap();
    }

    #[test]
    fn rebinding_stop_signal_isolates_recording_sessions() {
        let controller = HotkeyController::new(Shortcuts::default());
        let old = Arc::new(AtomicBool::new(false));
        let current = Arc::new(AtomicBool::new(false));
        controller.set_stop_signal(old.clone());
        controller.set_stop_signal(current.clone());
        signal_stop(&old);
        assert!(!current.load(Ordering::Acquire));
        signal_stop(&controller.stop.lock().unwrap());
        assert!(current.load(Ordering::Acquire));
    }

    #[test]
    fn stop_hotkey_sets_direct_capture_signal() {
        let stop = AtomicBool::new(false);
        signal_stop(&stop);
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn shortcuts_validate_keys_and_conflicts() {
        let mut shortcuts = Shortcuts::default();
        assert!(shortcuts.is_valid());
        assert_eq!(shortcuts.start.label(), "Ctrl+F9");
        shortcuts.stop = shortcuts.start;
        assert!(!shortcuts.is_valid());
        assert!(Shortcut::from_input(Key::A, Modifiers::NONE).is_none());
        assert!(Shortcut::from_input(Key::F4, Modifiers::ALT).is_none());
        assert_eq!(
            Shortcut::from_input(Key::R, Modifiers::CTRL | Modifiers::SHIFT)
                .unwrap()
                .label(),
            "Ctrl+Shift+R"
        );
    }

    #[test]
    fn modifiers_match_exactly() {
        let shortcut = Shortcuts::default().start;
        assert!(shortcut.matches(true, true, false, false));
        assert!(!shortcut.matches(true, true, false, true));
        assert!(!shortcut.matches(false, true, false, false));
    }

    #[test]
    fn editing_and_rebinding_wait_for_key_release() {
        let controller = HotkeyController::new(Shortcuts::default());
        let mut edges = KeyEdges::default();
        edges.poll(controller.snapshot(), false, false);
        assert_eq!(
            edges.poll(controller.snapshot(), true, false),
            (true, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), true, false),
            (false, false)
        );
        controller.suspend(true);
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, false)
        );
        controller.update(Shortcuts::default());
        controller.suspend(false);
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), false, false),
            (false, false)
        );
        assert_eq!(
            edges.poll(controller.snapshot(), false, true),
            (false, true)
        );
    }
}
