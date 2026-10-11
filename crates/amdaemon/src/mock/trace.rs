//! The chronological record the scripted doubles of one test write into.

use std::cell::RefCell;
use std::rc::Rc;

/// Chronological record shared by all doubles of one test.
pub(crate) type Trace = Rc<RefCell<Vec<String>>>;

pub(crate) fn new_trace() -> Trace {
    Rc::new(RefCell::new(Vec::new()))
}

pub(crate) fn push(trace: &Trace, line: impl Into<String>) {
    trace.borrow_mut().push(line.into());
}

pub(crate) fn lines(trace: &Trace) -> Vec<String> {
    trace.borrow().clone()
}
