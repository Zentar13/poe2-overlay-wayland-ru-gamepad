//! Gamepad control of the overlay card: right stick scrolls, D-pad up/down pages,
//! An optional trigger button (settings: Share / View / L3 / R3) runs a price check from
//! the pad — the overlay's own Ctrl+Alt+D. It is read while the card is closed too, when
//! the pad is NOT grabbed, so the game also sees that press: pick a button it ignores.
//!
//! left stick moves a selection over the search filters and X or A presses it (toggles the
//! filter / runs "Search again"), B closes.
//!
//! A controller player has no free hand for the mouse, so while the card is visible
//! the overlay drives it from the pad and keeps that input away from the game (B would
//! close the in-game inventory, the stick would move the camera).
//!
//! Two roles, possibly two devices:
//! - **input** — the physical pad, read non-exclusively. Steam and poe2-voice (Share)
//!   read it too, and it keeps delivering events whatever Steam does with its virtual
//!   pad while another surface is on top of the game.
//! - **blocker** — the device the game actually reads, grabbed (`EVIOCGRAB`) while the
//!   card is up. With Steam Input that is Steam's virtual pad (vendor 0x28de, "Microsoft
//!   X-Box 360 pad N"); grabbing it silences the game and leaves the physical pad — and
//!   so poe2-voice's Share — untouched. Without Steam Input the physical pad itself is
//!   grabbed; then poe2-voice can't see Share, so Share closes the card instead.
//!
//! The grab is released once the card is hidden *and* every button pressed during it
//! is up again, so the game never receives a release without its press.
//!
//! One polling thread (~60 Hz, non-blocking reads) owns both devices, follows the
//! window's visibility, and emits `gamepad-scroll` (pixels) to the webview. Devices
//! that disappear are rescanned every couple of seconds.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU16, Ordering};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};

use evdev::{AbsoluteAxisType, Device, InputEventKind, Key};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

const TICK: Duration = Duration::from_millis(16);
const RESCAN: Duration = Duration::from_secs(2);
/// Valve's USB vendor id — Steam Input's virtual controllers.
const STEAM_VENDOR: u16 = 0x28de;
/// Stick travel ignored around center (fraction of full deflection).
const DEADZONE: f64 = 0.2;
/// Scroll speed at full deflection, px per tick (~1500 px/s at 60 Hz).
const STICK_PX: f64 = 25.0;
/// One D-pad step, px.
const DPAD_PX: f64 = 120.0;
/// Left-stick deflection that counts as "up"/"down" for the filter selection.
const NAV_THRESHOLD: f64 = 0.5;
/// Held stick: first repeat after this, then every `NAV_REPEAT` (key-repeat feel).
const NAV_DELAY: Duration = Duration::from_millis(400);
const NAV_REPEAT: Duration = Duration::from_millis(120);

/// Key code of the price-check trigger button, 0 = none (set from the settings).
static TRIGGER: AtomicU16 = AtomicU16::new(0);

/// Trigger choices offered in the settings, by id.
const TRIGGERS: &[(&str, Key)] = &[
    ("share", Key::KEY_RECORD),
    ("select", Key::BTN_SELECT),
    ("l3", Key::BTN_THUMBL),
    ("r3", Key::BTN_THUMBR),
];

/// Set the price-check trigger by settings id (`"share"`, `"select"`, `"l3"`, `"r3"`);
/// anything else (`"none"`, `None`) disables it.
pub fn set_trigger(id: Option<&str>) {
    let code = id
        .and_then(|id| TRIGGERS.iter().find(|(t, _)| *t == id))
        .map_or(0, |(_, k)| k.code());
    TRIGGER.store(code, Ordering::Relaxed);
}

/// Start the gamepad thread.
pub fn spawn(app: AppHandle) {
    thread::spawn(move || run(app));
}

fn run(app: AppHandle) {
    let mut st = State::default();
    let mut last_scan: Option<Instant> = None;
    loop {
        // (Re)discover devices between cards: the physical pad may be replugged, and
        // Steam creates/destroys its virtual pad as games start and stop.
        if !st.grabbed && last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
            last_scan = Some(Instant::now());
            st.rescan();
        }
        if let Some(win) = app.get_webview_window("main") {
            if st.tick(&win) {
                // Off this thread: price_check sleeps while the game copies the item.
                let app = app.clone();
                thread::spawn(move || crate::hotkey::price_check(&app));
            }
        }
        thread::sleep(TICK);
    }
}

#[derive(Default)]
struct State {
    input: Option<Pad>,
    /// Steam's virtual pad, when present — grabbed instead of `input`.
    steam: Option<Pad>,
    grabbed: bool,
    /// Right stick Y, normalized to -1.0 (up) … 1.0 (down).
    ry: f64,
    /// Left stick Y, same scale.
    ly: f64,
    /// Direction the left stick is held in (-1 / 0 / 1) and when it next repeats.
    nav_dir: i32,
    nav_next: Option<Instant>,
    /// Input buttons pressed while grabbed and not yet released.
    held: HashSet<u16>,
}

struct Pad {
    dev: Device,
    name: String,
    ry_min: i32,
    ry_max: i32,
}

impl State {
    /// Fill in whichever device is missing (a vanished one is dropped by the read
    /// error in `tick`).
    fn rescan(&mut self) {
        if self.input.is_some() && self.steam.is_some() {
            return;
        }
        let (phys, steam) = find_pads();
        if self.input.is_none() {
            if let Some(p) = &phys {
                eprintln!("[gamepad] input: {}", p.name);
            }
            self.input = phys;
        }
        if self.steam.is_none() {
            if let Some(p) = &steam {
                eprintln!("[gamepad] blocking via Steam Input pad: {}", p.name);
            }
            self.steam = steam;
        }
    }

    /// The device to grab: Steam's virtual pad if any, else the physical one.
    fn blocker(&mut self) -> Option<&mut Pad> {
        if self.steam.is_some() {
            self.steam.as_mut()
        } else {
            self.input.as_mut()
        }
    }

    /// One poll; returns `true` when the price-check trigger button was pressed.
    fn tick(&mut self, win: &WebviewWindow) -> bool {
        let visible = win.is_visible().unwrap_or(false);
        let trigger = TRIGGER.load(Ordering::Relaxed);
        let mut triggered = false;
        // Share closes the card only when the pad is grabbed and Share isn't the trigger.
        let share_closes = self.steam.is_none() && trigger != Key::KEY_RECORD.code();

        // Drain the blocker's queue too, so nothing queued while grabbed replays later.
        if let Some(s) = self.steam.as_mut() {
            if drain(&mut s.dev).is_err() {
                eprintln!("[gamepad] Steam Input pad gone; blocking the physical pad");
                self.steam = None;
            }
        }

        let mut close = false;
        let mut select = false;
        let mut step = 0.0;
        let mut lost = false;
        if let Some(p) = self.input.as_mut() {
            let (ry_min, ry_max) = (p.ry_min, p.ry_max);
            match p.dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        match ev.kind() {
                            InputEventKind::AbsAxis(AbsoluteAxisType::ABS_RY) => {
                                self.ry = normalize(ev.value(), ry_min, ry_max);
                            }
                            // Both sticks share the axis range on these pads.
                            InputEventKind::AbsAxis(AbsoluteAxisType::ABS_Y) => {
                                self.ly = normalize(ev.value(), ry_min, ry_max);
                            }
                            InputEventKind::AbsAxis(AbsoluteAxisType::ABS_HAT0Y) if self.grabbed => {
                                step += ev.value() as f64 * DPAD_PX;
                            }
                            InputEventKind::Key(k) if trigger != 0 && k.code() == trigger => {
                                if ev.value() == 1 {
                                    triggered = true;
                                }
                                // Track it like any key so the grab isn't released mid-press.
                                if self.grabbed {
                                    if ev.value() == 1 {
                                        self.held.insert(k.code());
                                    } else if ev.value() == 0 {
                                        self.held.remove(&k.code());
                                    }
                                }
                            }
                            InputEventKind::Key(k) if self.grabbed => {
                                if ev.value() == 1 {
                                    self.held.insert(k.code());
                                    if k == Key::BTN_EAST || (share_closes && k == Key::KEY_RECORD) {
                                        close = true;
                                    }
                                    // X or A presses the selection. xpad reports the physical X
                                    // button as BTN_X (0x133), which the evdev crate only names by
                                    // its alias BTN_NORTH; A is BTN_SOUTH.
                                    if k == Key::BTN_NORTH || k == Key::BTN_SOUTH {
                                        select = true;
                                    }
                                } else if ev.value() == 0 {
                                    self.held.remove(&k.code());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    eprintln!("[gamepad] {} lost ({e})", p.name);
                    lost = true;
                }
            }
        }
        if lost {
            self.input = None;
            self.ry = 0.0;
            self.ly = 0.0;
        }

        if close && visible {
            let _ = win.hide();
            return triggered;
        }

        if visible && !self.grabbed {
            if let Some(b) = self.blocker() {
                match b.dev.grab() {
                    Ok(()) => {
                        self.grabbed = true;
                        self.held.clear();
                    }
                    Err(e) => eprintln!("[gamepad] grab failed: {e}"),
                }
            }
        } else if !visible && self.grabbed && self.held.is_empty() {
            if let Some(b) = self.blocker() {
                let _ = b.dev.ungrab();
            }
            self.grabbed = false;
        }

        if visible && self.grabbed {
            let mag = self.ry.abs();
            if mag > DEADZONE {
                let t = (mag - DEADZONE) / (1.0 - DEADZONE);
                step += self.ry.signum() * t * t * STICK_PX;
            }
            if step != 0.0 {
                let _ = win.emit("gamepad-scroll", step);
            }
            if let Some(dir) = self.nav_step() {
                let _ = win.emit("gamepad-nav", dir);
            }
            if select {
                let _ = win.emit("gamepad-select", ());
            }
        } else {
            self.nav_dir = 0;
        }
        triggered
    }

    /// Left stick → one selection step per push, auto-repeating while held.
    fn nav_step(&mut self) -> Option<i32> {
        let dir = if self.ly > NAV_THRESHOLD {
            1
        } else if self.ly < -NAV_THRESHOLD {
            -1
        } else {
            0
        };
        let now = Instant::now();
        if dir == 0 {
            self.nav_dir = 0;
            return None;
        }
        if dir != self.nav_dir {
            self.nav_dir = dir;
            self.nav_next = Some(now + NAV_DELAY);
            return Some(dir);
        }
        if self.nav_next.is_some_and(|t| now >= t) {
            self.nav_next = Some(now + NAV_REPEAT);
            return Some(dir);
        }
        None
    }
}

/// Discard pending events; `Err` = device gone.
fn drain(dev: &mut Device) -> std::io::Result<()> {
    match dev.fetch_events() {
        Ok(events) => {
            events.for_each(drop);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e),
    }
}

/// (physical pad, Steam virtual pad): gamepads with a B button and a right stick,
/// skipping our own uinput keyboard and ydotoold.
fn find_pads() -> (Option<Pad>, Option<Pad>) {
    let (mut phys, mut steam) = (None, None);
    for (_, dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("").to_string();
        if name.contains("poe2-overlay") || name.to_lowercase().contains("ydotool") {
            continue;
        }
        let has_b = dev.supported_keys().is_some_and(|k| k.contains(Key::BTN_EAST));
        let has_ry = dev
            .supported_absolute_axes()
            .is_some_and(|a| a.contains(AbsoluteAxisType::ABS_RY));
        if !(has_b && has_ry) {
            continue;
        }
        let is_steam = dev.input_id().vendor() == STEAM_VENDOR;
        if (is_steam && steam.is_some()) || (!is_steam && phys.is_some()) {
            continue;
        }
        let Some(pad) = Pad::open(dev, name) else { continue };
        if is_steam {
            steam = Some(pad);
        } else {
            phys = Some(pad);
        }
    }
    (phys, steam)
}

impl Pad {
    fn open(dev: Device, name: String) -> Option<Pad> {
        // Non-blocking so the tick loop can drain without stalling.
        let fd = dev.as_raw_fd();
        // SAFETY: fcntl on a valid fd owned by `dev`; only adds O_NONBLOCK.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return None;
            }
        }
        let (ry_min, ry_max) = dev
            .get_abs_state()
            .ok()
            .map(|s| {
                let i = &s[AbsoluteAxisType::ABS_RY.0 as usize];
                (i.minimum, i.maximum)
            })
            .filter(|(lo, hi)| hi > lo)
            .unwrap_or((-32768, 32767));
        Some(Pad { dev, name, ry_min, ry_max })
    }
}

/// Map a raw axis value to -1.0 … 1.0 around the axis center.
fn normalize(v: i32, min: i32, max: i32) -> f64 {
    let center = (min as f64 + max as f64) / 2.0;
    let half = (max as f64 - min as f64) / 2.0;
    ((v as f64 - center) / half).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalizes_signed_and_unsigned_axes() {
        assert!((normalize(0, -32768, 32767)).abs() < 0.001);
        assert!((normalize(32767, -32768, 32767) - 1.0).abs() < 0.001);
        assert!((normalize(-32768, -32768, 32767) + 1.0).abs() < 0.001);
        assert!((normalize(0, 0, 1023) + 1.0).abs() < 0.001);
    }
}
