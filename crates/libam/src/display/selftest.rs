//! The self-test (`can.md` 3.4.1, 3.4.2, 3.6, 3.7): `DisplayTester` under the command sequencer, and the
//! `CompleteTask` event it raises when it ends.

use libam_api::interface::LibamEvent;
use libam_api::result::{AmError, AmResult};

use super::{Display, Io};

/// The pattern of one test line: 80 vertical bars, which scroll on the device.
const PATTERN_LEN: usize = 80;
/// Whole seconds a pattern stays on a line.
const STEP_SECONDS: u64 = 5;
/// Test lines (the pattern visits line 0, then line 1).
const TEST_LINES: usize = 2;

/// The state of the tester (the base state numbers 0, 1, 3, 4; 2 and 5 are never used by it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum TesterState {
    #[default]
    Idle,
    Running,
    Cancelling,
    Ended,
}

/// `DisplayTester`.
#[derive(Debug, Default)]
pub(super) struct Tester {
    state: TesterState,
    index: usize,
    timer_ms: u64,
}

/// The sequencer state: 0 idle, 1 running, 3 cancelling, 4 ended, 5 aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SequencerState {
    #[default]
    Idle,
    Running,
    Cancelling,
    Ended,
    Aborted,
}

/// The command sequencer; the tester is its only child.
#[derive(Debug, Default)]
pub(super) struct Sequencer {
    state: SequencerState,
}

impl Sequencer {
    /// State 1..3.
    pub fn running(&self) -> bool {
        matches!(self.state, SequencerState::Running | SequencerState::Cancelling)
    }

    fn finished(&self) -> bool {
        matches!(self.state, SequencerState::Ended | SequencerState::Aborted)
    }
}

impl Tester {
    /// `Command::cancel`: accepted in state 1, then the cancel hook forces the test index to the end.
    fn cancel(&mut self) {
        if self.state == TesterState::Running {
            self.state = TesterState::Cancelling;
            self.index = TEST_LINES;
        }
    }
}

impl Display {
    /// `0x140265F20`: the pattern on line `index` of screen A, the other line blank; resets the timer.
    fn paint_test(&mut self, now_ms: u64) {
        self.screen_a.clear_pending();
        let pattern = "|".repeat(PATTERN_LEN);
        self.screen_a.write(self.tester.index, &pattern, false);
        self.tester.timer_ms = now_ms;
    }

    /// `display.startTest` (`0x14022DD30`): `Err(6)` when the display is not usable, `Err(3)` when busy or in use.
    /// Nothing is written to the device; the next frame draws the pattern.
    ///
    /// `now_ms` is the millisecond tick (`clock.tick_ms()`).
    pub fn start_test(&mut self, now_ms: u64) -> AmResult<()> {
        if !self.acquired {
            tracing::error!("display: the panel is not available");
            return Err(AmError::code(AmError::FAILED));
        }
        if !self.life.state.is_done() {
            tracing::error!("display: the module is still setting up");
            return Err(AmError::code(AmError::BUSY));
        }
        if self.mode_a {
            tracing::error!("display: the panel is already in use");
            return Err(AmError::code(AmError::BUSY));
        }
        self.mode_a = true;
        self.mode_changed = true;
        self.seq.state = SequencerState::Running;
        self.tester = Tester { state: TesterState::Running, index: 0, timer_ms: now_ms };
        self.paint_test(now_ms);
        self.screen_a.owned = true;
        Ok(())
    }

    /// `display.cancel` (`0x14022DAC0`): cancels the running test; it ends, and raises `CompleteTask`, on the next
    /// frame. No effect when no test owns the screen.
    pub fn cancel_test(&mut self) {
        if self.screen_a.owned {
            self.tester.cancel();
        }
    }

    /// `updateSequencer` (`0x14022E0F0`).
    pub(super) fn update_sequencer(&mut self, io: &mut Io<'_>) {
        let ended_before = self.seq.finished();
        self.tick_sequencer(io);
        if self.seq.running() && self.screen_a.active {
            // The module was restarted while a test ran.
            if self.screen_a.owned {
                self.cancel_sequencer();
            }
        } else if !ended_before && self.seq.finished() {
            self.screen_a.active = false;
            self.screen_b.active = false;
            self.mode_changed = self.mode_a;
            self.mode_a = false;
            self.screen_a.clear_all();
        }
    }

    /// The sequencer's own cancel: state 1 or 2 to 3, and the current child is cancelled.
    pub(super) fn cancel_sequencer(&mut self) {
        if self.seq.state == SequencerState::Running {
            self.seq.state = SequencerState::Cancelling;
            self.tester.cancel();
        }
    }

    /// `0x140256740`: one tick of the sequencer over its single child.
    fn tick_sequencer(&mut self, io: &mut Io<'_>) {
        if !self.seq.running() {
            return;
        }
        let now_ms = io.clock.tick_ms();
        match self.tester.state {
            TesterState::Cancelling => self.tester.state = TesterState::Ended,
            TesterState::Running => {
                if now_ms.saturating_sub(self.tester.timer_ms) / 1000 >= STEP_SECONDS {
                    self.tester.index += 1;
                    if self.tester.index >= TEST_LINES {
                        self.tester.state = TesterState::Ended;
                    } else {
                        self.paint_test(now_ms);
                    }
                }
            }
            TesterState::Idle | TesterState::Ended => {}
        }
        if self.tester.state == TesterState::Ended {
            self.end_test(io);
            // The tester is the last child: a cancelled sequencer aborts, otherwise it ends.
            self.seq.state = if self.seq.state == SequencerState::Cancelling {
                SequencerState::Aborted
            } else {
                SequencerState::Ended
            };
        }
    }

    /// `DisplayTester::end` (`0x140265E00`): blanks screen A, releases it and notifies `CompleteTask(1, 0)`.
    fn end_test(&mut self, io: &mut Io<'_>) {
        self.screen_a.clear_pending();
        self.screen_a.owned = false;
        self.tester.state = TesterState::Idle;
        if self.config.complete_task_listener {
            io.events.push(LibamEvent::DisplayCompleteTask);
        }
    }
}
