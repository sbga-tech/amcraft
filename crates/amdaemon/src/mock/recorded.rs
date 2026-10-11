//! Records the libam events handed to the App while delegating to the real interface.

use std::cell::RefCell;
use std::rc::Rc;

use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam_api::result::AmResult;
use libam_api::sequence::ProcessState;

/// What the daemon-through-libam test observes of a libam event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    CabinetTypeRequest,
    ChangeState(u32),
    FatalError(u16),
    Other,
}

fn seen(event: &LibamEvent) -> Seen {
    match event {
        LibamEvent::LoadCabinetType(_) => Seen::CabinetTypeRequest,
        LibamEvent::ChangeState(ProcessState(state)) => Seen::ChangeState(*state),
        LibamEvent::FatalError(error) => Seen::FatalError(error.number),
        _ => Seen::Other,
    }
}

/// libam's interface with every event it hands the daemon recorded on the way.
#[derive(Debug)]
pub(crate) struct Recorded {
    pub inner: Box<dyn AmInterface>,
    pub events: Rc<RefCell<Vec<Seen>>>,
}

impl AmInterface for Recorded {
    fn initialize(&mut self, config: &LibamwConfig) -> AmResult<Vec<LibamEvent>> {
        let events = self.inner.initialize(config)?;
        self.events.borrow_mut().extend(events.iter().map(seen));
        Ok(events)
    }

    fn execute(&mut self) -> Vec<LibamEvent> {
        let events = self.inner.execute();
        self.events.borrow_mut().extend(events.iter().map(seen));
        events
    }

    fn load_cabinet_type_result(&mut self, accepted: bool, app_config: CabinetAppConfig) {
        self.inner.load_cabinet_type_result(accepted, app_config);
    }

    fn finalize(&mut self) {
        self.inner.finalize();
    }

    fn is_develop(&self) -> AmResult<bool> {
        self.inner.is_develop()
    }

    fn app_root_path(&self) -> AmResult<Option<String>> {
        self.inner.app_root_path()
    }
}
