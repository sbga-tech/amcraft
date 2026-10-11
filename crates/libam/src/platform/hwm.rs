//! The hardware monitor (`am::hwm::Manager`, `spec/libam/platform.md` part D 3.2, 4.2, 4.3 and 5.8): the CPU
//! temperature of the `sghwmonitor` driver, sampled once a minute against the threshold of the registry value
//! `CpuTempError`. Two consecutive over-threshold samples raise error 91; a monitor that cannot start raises 4105.
//!
//! The NVRAM context that `hwm::Context::initialize` also acquires is not modelled: the platform manager holds
//! it for the whole run, and its failure is already error 4105 there.

use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::registry_keys::STATIC_KEY;
use backend::{HwMonitor, HwmError, KeyMode, RegValue};

/// The CPU over-temperature error.
pub const ERROR_OVER_TEMPERATURE: u16 = 91;
/// The monitor could not be started.
pub const ERROR_NOT_STARTED: u16 = 4105;

/// The registry value holding the threshold (`amsSystemPropertyGetCpuTempError`).
const THRESHOLD_VALUE: &str = "CpuTempError";
/// Seconds between two samples.
const SAMPLE_PERIOD_SECONDS: u64 = 60;
/// Consecutive over-threshold samples that make an error.
const OVER_SAMPLES: u32 = 2;
/// The range of a believable reading (`amyHmReadCpuTemp`): `-22..=105`.
const VALID_TEMPERATURE: std::ops::RangeInclusive<i32> = -22..=105;

/// Why `amHm*` did not succeed; [`HmError::code`] is the original's return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum HmError {
    /// `-3`: not initialised.
    #[error("the hardware monitor is not initialised")]
    NotInitialised,
    /// `-5`: any failure of the driver.
    #[error("the hardware monitor failed")]
    Failed,
}

impl HmError {
    const fn code(self) -> i32 {
        match self {
            Self::NotInitialised => -3,
            Self::Failed => -5,
        }
    }
}

/// What a sample step found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing wrong.
    Quiet,
    /// The reading failed (`-5`).
    Failed,
    /// Over the threshold twice in a row (`4`).
    Over,
}

/// The sampler's phase (`0x140969AD8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    /// A sample is due.
    #[default]
    Sample,
    /// Over-temperature found; reported at the next step.
    Reported,
    /// The cycle is finished.
    Finished,
}

/// The C layer state `amHm` (`0x140969AA8`).
#[derive(Debug)]
struct Sampler {
    threshold: i32,
    over_count: u32,
    last_sample_ms: u64,
    cycle_error: i32,
    last_result: i32,
    phase: Phase,
}

impl Sampler {
    /// One `amHmCheckCpuTemp` step.
    fn step(&mut self, device: &mut dyn HwMonitor) -> (Step, Phase) {
        match self.phase {
            Phase::Sample => match read_temperature(device) {
                Err(_) => (Step::Failed, Phase::Finished),
                Ok(temperature) if self.threshold > temperature => {
                    self.over_count = 0;
                    (Step::Quiet, Phase::Finished)
                }
                Ok(_) => {
                    self.over_count = self.over_count.saturating_add(1);
                    if self.over_count < OVER_SAMPLES {
                        (Step::Quiet, Phase::Finished)
                    } else {
                        (Step::Over, Phase::Reported)
                    }
                }
            },
            Phase::Reported => (Step::Quiet, Phase::Finished),
            Phase::Finished => (Step::Failed, Phase::Finished),
        }
    }

    /// `amHmUpdate(0)`: the cached result until a minute has passed, then one step per call.
    fn update(&mut self, device: &mut dyn HwMonitor, now_ms: u64) -> i32 {
        if now_ms.saturating_sub(self.last_sample_ms) / 1000 < SAMPLE_PERIOD_SECONDS {
            return self.last_result;
        }
        let (step, next) = self.step(device);
        self.phase = next;
        let result = match step {
            Step::Quiet => 0,
            Step::Failed => HmError::Failed.code(),
            Step::Over => 4,
        };
        if result != 0 && self.cycle_error == 0 {
            self.cycle_error = result;
        }
        if self.phase == Phase::Finished {
            self.last_sample_ms = now_ms;
            self.phase = Phase::Sample;
            self.last_result = self.cycle_error;
            self.cycle_error = 0;
            return self.last_result;
        }
        0
    }
}

/// `amyHmReadCpuTemp`: the reading, or an error when it is outside `-22..=105`.
fn read_temperature(device: &mut dyn HwMonitor) -> Result<i32, HwmError> {
    let temperature = device.read_cpu_temperature()?;
    if VALID_TEMPERATURE.contains(&temperature) { Ok(temperature) } else { Err(HwmError::Read) }
}

/// `am::hwm::Manager` (`ctx+126216`).
#[derive(Debug, Default)]
pub struct Hwm {
    life: Lifecycle,
    /// `+8`: the context handle is held, so `amHmInit` succeeded.
    has_handle: bool,
    sampler: Option<Sampler>,
}

impl Hwm {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Manager::setup` (`0x14029B6E0`), from `OperatingState::enter`: `amHmInit`, and the state becomes 1 whatever
    /// the result.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        if !self.has_handle {
            match self.start(cx) {
                Ok(()) => self.has_handle = true,
                Err(error) => tracing::warn!(code = error.code(), "hardware monitor: opening failed"),
            }
        }
        self.life.begin();
    }

    /// `amHmInit` (`0x1402D8E30`): the threshold, the driver, one trial reading.
    fn start(&mut self, cx: &mut Cx<'_>) -> Result<(), HmError> {
        let threshold = match cx.backend.registry.read(STATIC_KEY, KeyMode::CreateVolatile, THRESHOLD_VALUE) {
            Ok(RegValue::Dword(value)) => value.cast_signed(),
            _ => return Err(HmError::Failed),
        };
        let device = &mut *cx.backend.hwm;
        device.open().map_err(|_| HmError::Failed)?;
        let temperature = match read_temperature(device) {
            Ok(temperature) => temperature,
            Err(_) => {
                device.close();
                return Err(HmError::Failed);
            }
        };
        self.sampler = Some(Sampler {
            threshold,
            over_count: u32::from(threshold <= temperature),
            last_sample_ms: cx.clock.tick_ms(),
            cycle_error: 0,
            last_result: 0,
            phase: Phase::Sample,
        });
        Ok(())
    }

    /// `Manager::update` (`0x14029B820`, 4.2), once per Operating frame after the dongle.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if !self.life.error.is_empty() {
            return;
        }
        match self.life.state {
            StateWord::INITIALIZED => {
                if self.has_handle {
                    self.life.state = StateWord::READY;
                } else {
                    self.life.error.set(ERROR_NOT_STARTED, 0);
                    self.life.state = StateWord::FAILED;
                }
            }
            StateWord::READY => {
                let now = cx.clock.tick_ms();
                let result = match &mut self.sampler {
                    Some(sampler) => Ok(sampler.update(&mut *cx.backend.hwm, now)),
                    None => Err(HmError::NotInitialised),
                };
                match result {
                    Ok(4) => self.life.error.set(ERROR_OVER_TEMPERATURE, 0),
                    Ok(code) if code < 0 => tracing::info!(code, "hardware monitor: update failed"),
                    Ok(_) => {}
                    Err(error) => tracing::info!(code = error.code(), "hardware monitor: update failed"),
                }
            }
            _ => {}
        }
    }

    /// `0x14029B7E0`, from the main-flow terminate: releases the handle (`amHmExit`), state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.has_handle {
            cx.backend.hwm.close();
            self.has_handle = false;
            self.sampler = None;
        }
        self.life.end();
    }

    /// `0x14029B950` (unconditional): clears the latched error and returns to state 1; the handle is kept.
    pub fn restart(&mut self) {
        self.life.begin();
    }

    /// `ctx+126232`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state in {2, 3}.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`ctx+126236`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::mock::SimHwMonitor;

    /// A sampler with a threshold of 80 whose last sample was at tick 0.
    fn sampler(over_count: u32) -> Sampler {
        Sampler { threshold: 80, over_count, last_sample_ms: 0, cycle_error: 0, last_result: 0, phase: Phase::Sample }
    }

    fn hot() -> SimHwMonitor {
        let mut device = SimHwMonitor::new();
        device.open().expect("opens");
        device.set_temperature(Some(90));
        device
    }

    #[test]
    fn nothing_is_sampled_within_a_minute() {
        let mut device = hot();
        let mut sampler = sampler(1);
        assert_eq!(sampler.update(&mut device, 59_999), 0);
        assert_eq!(sampler.over_count, 1);
    }

    #[test]
    fn two_hot_samples_are_reported_one_step_after_the_second() {
        let mut device = hot();
        let mut sampler = sampler(0);
        // First hot sample: counted, the cycle is finished with nothing to report.
        assert_eq!(sampler.update(&mut device, 60_000), 0);
        // Second hot sample: found at once, reported by the following step.
        assert_eq!(sampler.update(&mut device, 120_000), 0);
        assert_eq!(sampler.update(&mut device, 120_001), 4);
        // The result is cached for the next minute.
        assert_eq!(sampler.update(&mut device, 130_000), 4);
        assert_eq!(sampler.last_sample_ms, 120_001);
    }

    #[test]
    fn a_cool_sample_resets_the_count() {
        let mut device = hot();
        let mut sampler = sampler(1);
        device.set_temperature(Some(50));
        assert_eq!(sampler.update(&mut device, 60_000), 0);
        assert_eq!(sampler.over_count, 0);
    }

    #[test]
    fn a_failed_reading_is_the_cached_failure_for_a_minute() {
        let mut device = hot();
        let mut sampler = sampler(0);
        device.set_temperature(None);
        assert_eq!(sampler.update(&mut device, 60_000), -5);
        assert_eq!(sampler.update(&mut device, 60_500), -5);
    }

    #[test]
    fn out_of_range_readings_fail() {
        let mut device = hot();
        for reading in [-23, 106] {
            device.set_temperature(Some(reading));
            assert_eq!(read_temperature(&mut device), Err(HwmError::Read));
        }
        for reading in [-22, 105] {
            device.set_temperature(Some(reading));
            assert_eq!(read_temperature(&mut device), Ok(reading));
        }
    }
}
