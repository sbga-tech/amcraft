//! [`Console`] double: records allocation and release; allocation always succeeds.

use std::rc::Rc;

use crate::daemon::console::Console;
use crate::mock::trace::{Trace, push};

/// Recording console.
#[derive(Debug)]
pub(crate) struct FakeConsole {
    trace: Trace,
}

impl FakeConsole {
    pub(crate) fn new(trace: &Trace) -> Self {
        Self { trace: Rc::clone(trace) }
    }
}

impl Console for FakeConsole {
    fn allocate(&mut self, title: &str) -> bool {
        push(&self.trace, format!("console.allocate({title:?})"));
        true
    }

    fn free(&mut self) {
        push(&self.trace, "console.free");
    }
}
