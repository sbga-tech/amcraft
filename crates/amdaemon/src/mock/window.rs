//! [`MessageWindow`] double: a recorded window creation and a scripted message pump.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::daemon::window::{MessagePump, MessageWindow};
use crate::mock::trace::{Trace, push};

/// Scripted window. The pump answers from `pump`, then [`MessagePump::Idle`].
#[derive(Debug)]
pub(crate) struct FakeWindow {
    trace: Trace,
    pub pump: VecDeque<MessagePump>,
    /// Result of [`MessageWindow::create_hidden`].
    pub creatable: bool,
    turns: usize,
}

impl FakeWindow {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self { trace: Rc::clone(trace), pump: VecDeque::new(), creatable: true, turns: 0 }
    }
}

impl MessageWindow for FakeWindow {
    fn create_hidden(&mut self, _class_name: &str, _title: &str) -> bool {
        push(&self.trace, "window.create");
        self.creatable
    }

    fn pump(&mut self) -> MessagePump {
        self.turns += 1;
        assert!(self.turns < 2_000, "runaway main loop: the test never ended the Manager state");
        self.pump.pop_front().unwrap_or(MessagePump::Idle)
    }
}
