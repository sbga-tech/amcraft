//! The customer display (`am::display::Manager` and the e-payment screen, `spec/libam/can.md` section 3): the VFD
//! driver, its self-test and the renderer. There is no thread; everything runs from [`Display::update`].
//!
//! # Cross-module methods
//!
//! E-money and Aime Pay call these on `core.display` (inside `shared.with(|core| ..)`); none needs the backend,
//! except [`Display::start_test`], which takes the tick and the event queue.
//!
//! | Method | Original | Meaning |
//! |---|---|---|
//! | [`Display::ready`] | `0x14022DF70` | enabled, state 2 or 3, and the VFD usable: the entry check of the payment managers |
//! | [`Display::is_ready`] | `0x14022DFF0` | state 2 or 3: the pump's start/restart condition and the `allReady` term |
//! | [`Display::is_mode_a`] | `0x14022E010` | screen A owns the VFD |
//! | [`Display::enter_mode_a`] | `0x14022DB00` | a transaction starts: screen A is shown |
//! | [`Display::leave_mode_a`] | `0x14022DB20` | the transaction ends: screen B is shown, screen A is cleared |
//! | [`Display::set_line`] | `0x14022DC50` | writes a line of screen A (ignored while a test runs) |
//! | [`Display::clear_lines`] | `0x14022DB50` | clears screen A (ignored while a test runs) |
//! | [`Display::start_test`] | `0x14022DD30` | `display.startTest` (`Err(6)` unusable, `Err(3)` busy/in use) |
//! | [`Display::cancel_test`] | `0x14022DAC0` | `display.cancel` |
//! | [`Display::lane_clear`], [`Display::lane_set_entry`], [`Display::lane_set_state`] | `0x14027ADA0`, `0x14027ADD0`, `0x14027AE40` | the e-payment lanes of screen B |
//!
//! The `CompleteTask` event (`LibamEvent::DisplayCompleteTask`) is pushed to `cx.events` by [`Display::update`] and
//! [`Display::terminate`] when a test ends [display.event.complete_task].

mod context;
mod lanes;
mod screen;
mod selftest;
#[cfg(test)]
mod tests;
mod vfd;

use libam_api::am_config::DisplayConfig;

use self::context::Context;
use self::lanes::Lanes;
use self::screen::Screen;
use self::selftest::{Sequencer, Tester};
use crate::clock::Clock;
use crate::module::{Cx, ErrorCell, EventQueue, Lifecycle, StateWord};
use backend::SerialPorts;

pub use lanes::Lane;
pub use vfd::{SendStatus, Vfd, VfdError};

/// Shutdown flush: at most this many iterations of one millisecond, twice (`can.md` 3.3.6).
const FLUSH_ITERATIONS: u32 = 60;

/// What the frame passes down while it runs a screen step: the ports, the event queue and the clock.
struct Io<'a> {
    ports: &'a dyn SerialPorts,
    events: &'a mut EventQueue,
    clock: &'a dyn Clock,
}

impl<'a> Io<'a> {
    fn new(cx: &'a mut Cx<'_>) -> Self {
        Self { ports: &*cx.backend.serial, events: &mut *cx.events, clock: cx.clock }
    }
}

/// `am::display::Manager` (`ctx+140656`) with the e-payment manager (`ctx+140648`).
#[derive(Debug, Default)]
pub struct Display {
    life: Lifecycle,
    config: DisplayConfig,
    ctx: Context,
    /// `acquired`: this manager holds the VFD context.
    acquired: bool,
    mode_a: bool,
    mode_changed: bool,
    screen_a: Screen,
    screen_b: Screen,
    /// `bMessagesEnabled`.
    messages_enabled: bool,
    lanes: Lanes,
    seq: Sequencer,
    tester: Tester,
}

impl Display {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` and `Manager::setup` (`0x14022D760`): takes `AppConfig.display`; state 1 when the
    /// display is enabled (the device is opened by the first frame), else state 2.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.display;
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x14022D9F0`, once per Operating frame, last before `allReady`.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if !self.config.enable {
            return;
        }
        let mut io = Io::new(cx);
        if !self.life.state.is_done() {
            self.open_context(&mut io);
        }
        self.update_sequencer(&mut io);
        self.update_display(&mut io);
    }

    /// `0x14022D7B0`: cancels a running test (its completion event fires here), blanks the VFD, flushes the last
    /// packet and closes the port.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        let mut io = Io::new(cx);
        if self.seq.running() {
            self.cancel_sequencer();
        }
        for _ in 0..FLUSH_ITERATIONS {
            if self.config.enable && !self.life.state.is_done() {
                self.open_context(&mut io);
            }
            self.update_sequencer(&mut io);
            self.update_display(&mut io);
            if !self.seq.running() {
                break;
            }
            io.clock.sleep_ms(1);
        }
        if !self.seq.running() {
            self.screen_a.clear_pending();
        }
        self.clear_screen_b();
        for _ in 0..FLUSH_ITERATIONS {
            self.update_sequencer(&mut io);
            self.update_display(&mut io);
            if !self.acquired || self.ctx.last_result != 1 {
                break;
            }
            io.clock.sleep_ms(1);
        }
        if self.acquired {
            self.release_handle(&mut io);
        }
        self.config = DisplayConfig::default();
        self.mode_a = false;
        self.mode_changed = false;
        self.screen_a.clear_all();
        self.screen_b.clear_all();
        self.life.end();
    }

    /// Module restart (`0x14022DA40`): clears the screens and goes back to state 1, so the next frame opens the
    /// device again if it was released.
    pub fn restart(&mut self) {
        if !self.config.enable {
            return;
        }
        if !self.mode_a {
            self.clear_lines();
        }
        self.clear_screen_b();
        self.life.error.clear();
        self.life.state = StateWord::INITIALIZED;
        if self.seq.running() {
            self.screen_a.active = true;
            self.screen_b.active = true;
        }
    }

    /// `0x14022DF70`: enabled, state 2 or 3 and the VFD usable (or a message listener installed).
    pub fn ready(&self) -> bool {
        self.config.enable && self.life.state.is_done() && (self.ctx.opened || self.config.message_listener)
    }

    /// `0x14022DFF0`: state 2 or 3.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// `0x14022E010`: screen A owns the VFD.
    pub fn is_mode_a(&self) -> bool {
        self.mode_a
    }

    /// `0x14022DB00`: screen A is shown; a switch from screen B forces a full redraw.
    pub fn enter_mode_a(&mut self) {
        self.mode_changed = !self.mode_a;
        self.mode_a = true;
    }

    /// `0x14022DB20`: screen B is shown again (full redraw) and screen A is cleared.
    pub fn leave_mode_a(&mut self) {
        self.mode_changed = self.mode_a;
        self.mode_a = false;
        self.clear_lines();
    }

    /// `0x14022DC50`: writes a line of screen A; `scroll` adds the six wrap-around spaces to a scrolling text.
    /// Ignored while a test runs, or for a line above 1.
    pub fn set_line(&mut self, line: usize, text: &str, scroll: bool) {
        if !self.seq.running() {
            self.screen_a.write(line, text, scroll);
        }
    }

    /// `0x14022DB50`: empties screen A. Ignored while a test runs.
    pub fn clear_lines(&mut self) {
        if !self.seq.running() {
            self.screen_a.clear_pending();
        }
    }

    /// `0x14027ADA0`: removes the entries of the lane.
    pub fn lane_clear(&mut self, lane: Lane) {
        self.lanes.clear(lane);
    }

    /// `0x14027ADD0`: adds the entry `id` to the lane or replaces its text.
    pub fn lane_set_entry(&mut self, lane: Lane, id: u32, text: &str) {
        self.lanes.set_entry(lane, id, text);
    }

    /// `0x14027AE40`: stores the lane state (0 unavailable, 1 available, 2 cabinet information mismatch,
    /// 3 payment failed, 4 blank) and re-renders screen B.
    pub fn lane_set_state(&mut self, lane: Lane, state: u32) {
        self.lanes.set_state(lane, state);
        let rendered = self.lanes.render(state);
        self.clear_screen_b();
        if let Some(line0) = rendered.line0 {
            self.screen_b.write(0, line0, true);
            self.screen_b.write(1, &rendered.line1, false);
        }
        self.messages_enabled = rendered.messages_enabled;
    }

    /// The error cell (`0x14022E020`); the manager never writes it.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// `display.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    /// `0x14022DBD0`: empties screen B and enables the messages.
    fn clear_screen_b(&mut self) {
        self.screen_b.clear_pending();
        self.messages_enabled = true;
    }

    /// `openContext` (`0x14022E030`).
    fn open_context(&mut self, io: &mut Io<'_>) {
        self.life.state = StateWord::READY;
        if self.acquired || self.config.display_port == 0 {
            return;
        }
        // The context is process-wide and this is its only holder: the reference count is `acquired`.
        if self.ctx.initialize(io.ports, self.config.display_port, io.clock.tick_ms()).is_err() {
            self.ctx.finalize();
            return;
        }
        self.acquired = true;
    }

    /// Releases the context handle; the last holder closes the port.
    fn release_handle(&mut self, _io: &mut Io<'_>) {
        self.acquired = false;
        self.ctx.finalize();
    }

    /// `updateDisplay` (`0x14022E200`).
    fn update_display(&mut self, io: &mut Io<'_>) {
        if self.acquired {
            self.ctx.track(io.clock.tick_ms());
            if self.ctx.last_result == 1 {
                return;
            }
            if !self.ctx.opened {
                self.release_handle(io);
            }
        }
        if !self.ready() {
            return;
        }
        let forced = self.acquired && self.ctx.last_result != 0;
        let changed_a = forced || self.screen_a.differs();
        let changed_b = forced || self.screen_b.differs();
        if changed_a {
            self.screen_a.commit();
        }
        if changed_b {
            self.screen_b.commit();
        }
        let switched = self.mode_changed;
        self.mode_changed = false;
        let changed = if self.mode_a { changed_a } else { changed_b };
        if !changed && !switched {
            return;
        }
        if !self.acquired {
            return;
        }
        let screen = if self.mode_a { &self.screen_a } else { &self.screen_b };
        let driver = &mut self.ctx.driver;
        if let Err(error) = driver.clear_message() {
            tracing::warn!(code = error.code(), "display: clearing the panel failed");
        }
        for (index, line) in screen.lines.iter().enumerate() {
            if line.pending.is_empty() {
                continue;
            }
            let (written, name) = if line.scroll {
                (driver.set_message_scroll(&line.pending, index), "SetMessageScroll")
            } else {
                (driver.set_message(&line.pending, index), "SetMessage")
            };
            if let Err(error) = written {
                tracing::warn!(line = index, op = name, code = error.code(), "display: writing a line failed");
            }
        }
        self.ctx.track(io.clock.tick_ms());
    }
}
