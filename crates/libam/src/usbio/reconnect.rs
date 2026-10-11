//! The driver conversation of a USB reconnect (`spec/libam/usbio.md` part 4, 7.2 and 7.3).
//!
//! The original runs it on a worker thread: write the target `PID` and `VID` as `REG_QWORD` values below
//! `HKLM\System\Sega\SystemProperty\sgworker`, wait until the helper service reports `STATE == 0` (idle), send the
//! user-defined control code `0x80` to the service `sgmanageio`, then poll `STATE` until it leaves 1 and read
//! `LAST_RESULT_CODE`. The fixed sleeps are 100 ms between polls and retries and 1000 ms after the control code, and
//! nothing ever times out.
//!
//! libam's backend handles are single-threaded (`Rc` handles), so the conversation is a step machine that
//! [`Job::advance`] moves on from the frame pump instead of a thread: every `Sleep` is a wake-up time on the
//! internal [`Clock`](crate::clock::Clock), so a test with the simulated clock reproduces it frame by frame.
//! The mutex-guarded registry access with its six attempts (7.2) belongs to the backend registry.

use backend::{Backend, KeyMode, RegValue, Registry};

/// The key of the helper service (7.4).
const KEY: &str = r"System\Sega\SystemProperty\sgworker";
/// The Windows service that does the reconnect.
const SERVICE: &str = "sgmanageio";
/// The user-defined control code "reconnect USB device `VID:PID`".
const CONTROL_RECONNECT: u32 = 0x80;
/// `Sleep(100)`: between polls and before every retry.
const POLL_MS: u64 = 100;
/// `Sleep(1000)` after the control code.
const SETTLE_MS: u64 = 1000;

/// The service state value that aborts the wait (7.4).
const STATE_FATAL: u16 = 2;
/// The service state value "busy" (7.4).
const STATE_BUSY: u16 = 1;
/// `LAST_RESULT_CODE` for "driver error" (7.7: handler result `0x81000015`).
const RESULT_DRIVER: u16 = 3;

/// How a reconnect ended (the amMaster outcome 0/1/2 of 6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `LAST_RESULT_CODE == 0`.
    Reconnected,
    /// Any other failure: registry or service errors, `STATE == 2`, an unknown `LAST_RESULT_CODE`.
    Failed,
    /// `LAST_RESULT_CODE == 3` (the handler result `0x81000015`).
    DriverError,
}

impl Outcome {
    /// The completion code the listener receives (4.3, 5): 1 ok, 2 failure, 3 driver error.
    pub fn listener_code(self) -> i32 {
        match self {
            Self::Reconnected => 1,
            Self::Failed => 2,
            Self::DriverError => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prop {
    Pid,
    Vid,
    State,
    LastResult,
}

impl Prop {
    fn name(self) -> &'static str {
        match self {
            Self::Pid => "PID",
            Self::Vid => "VID",
            Self::State => "STATE",
            Self::LastResult => "LAST_RESULT_CODE",
        }
    }
}

/// Where the conversation stands. `retry` marks the second attempt of an access (each is tried twice, 100 ms
/// apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    WritePid {
        retry: bool,
    },
    WriteVid {
        retry: bool,
    },
    /// Phase A: read `STATE` until the service is idle.
    PollIdle {
        retry: bool,
    },
    /// Phase A: the sleep after a successful poll, then the test of the value read.
    IdleDelay {
        state: u16,
    },
    /// `ControlService(0x80)`.
    Control,
    /// Phase B: read `STATE` until it leaves "busy".
    PollBusy {
        retry: bool,
    },
    /// Phase B: the sleep after a successful poll, then the test of the value read.
    BusyDelay {
        state: u16,
    },
    ReadResult {
        retry: bool,
    },
}

/// One reconnect in flight.
#[derive(Debug)]
pub struct Job {
    vid: u16,
    pid: u16,
    step: Step,
    wake_ms: u64,
    outcome: Option<Outcome>,
}

fn read_prop(registry: &mut dyn Registry, prop: Prop) -> Option<u16> {
    match registry.read(KEY, KeyMode::Open, prop.name()) {
        // The handler consumes the low 16 bits of the QWORD (7.2).
        Ok(RegValue::Qword(value)) => Some(value as u16),
        _ => None,
    }
}

impl Job {
    /// A reconnect of `vid:pid` that starts at the next [`advance`](Self::advance).
    pub fn new(vid: u16, pid: u16) -> Self {
        Self { vid, pid, step: Step::WritePid { retry: false }, wake_ms: 0, outcome: None }
    }

    /// The result once the conversation ended.
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// Runs every step whose wake-up time has come, until the job sleeps or ends.
    pub fn advance(&mut self, backend: &mut Backend, clock: &dyn crate::clock::Clock) {
        while self.outcome.is_none() {
            let now = clock.tick_ms();
            if now < self.wake_ms {
                return;
            }
            self.run(backend, now);
        }
    }

    fn sleep(&mut self, now: u64, ms: u64, next: Step) {
        self.wake_ms = now + ms;
        self.step = next;
    }

    fn fail(&mut self) {
        self.outcome = Some(Outcome::Failed);
    }

    /// One access with the retry rule: success runs `ok`, the first failure sleeps and tries again, the second
    /// ends the job as failed.
    fn run(&mut self, backend: &mut Backend, now: u64) {
        match self.step {
            Step::WritePid { retry } => {
                let written = write_prop(&mut *backend.registry, Prop::Pid, self.pid);
                self.after_write(now, written, retry, Step::WritePid { retry: true }, Step::WriteVid { retry: false });
            }
            Step::WriteVid { retry } => {
                let written = write_prop(&mut *backend.registry, Prop::Vid, self.vid);
                self.after_write(now, written, retry, Step::WriteVid { retry: true }, Step::PollIdle { retry: false });
            }
            Step::PollIdle { retry } => match read_prop(&mut *backend.registry, Prop::State) {
                None if !retry => self.sleep(now, POLL_MS, Step::PollIdle { retry: true }),
                None => self.fail(),
                Some(STATE_FATAL) => self.fail(),
                Some(state) => self.sleep(now, POLL_MS, Step::IdleDelay { state }),
            },
            Step::IdleDelay { state } => {
                self.step = if state == 0 { Step::Control } else { Step::PollIdle { retry: false } };
            }
            Step::Control => match backend.services.control(SERVICE, CONTROL_RECONNECT) {
                Ok(()) => self.sleep(now, SETTLE_MS, Step::PollBusy { retry: false }),
                Err(_) => self.fail(),
            },
            Step::PollBusy { retry } => match read_prop(&mut *backend.registry, Prop::State) {
                None if !retry => self.sleep(now, POLL_MS, Step::PollBusy { retry: true }),
                None => self.fail(),
                Some(state) => self.sleep(now, POLL_MS, Step::BusyDelay { state }),
            },
            Step::BusyDelay { state } => {
                self.step = if state == STATE_BUSY {
                    Step::PollBusy { retry: false }
                } else {
                    Step::ReadResult { retry: false }
                };
            }
            Step::ReadResult { retry } => match read_prop(&mut *backend.registry, Prop::LastResult) {
                None if !retry => self.sleep(now, POLL_MS, Step::ReadResult { retry: true }),
                None => self.fail(),
                Some(0) => self.outcome = Some(Outcome::Reconnected),
                Some(RESULT_DRIVER) => self.outcome = Some(Outcome::DriverError),
                Some(_) => self.fail(),
            },
        }
    }

    fn after_write(&mut self, now: u64, written: bool, retry: bool, again: Step, next: Step) {
        match (written, retry) {
            (true, _) => self.step = next,
            (false, false) => self.sleep(now, POLL_MS, again),
            (false, true) => self.fail(),
        }
    }
}

fn write_prop(registry: &mut dyn Registry, prop: Prop, value: u16) -> bool {
    registry.write(KEY, KeyMode::Open, prop.name(), &RegValue::Qword(u64::from(value))).is_ok()
}
