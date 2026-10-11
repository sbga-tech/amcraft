//! The special device manager (`am::credit::As6dbManager`, `credit.md` part II 3.2 and 3.12 to 3.15): driver
//! lifecycle, the lockout with its cooldown, the fault supervision and the coin counter cross-check against the
//! I/O board.

use crate::config::CreditAs6db;

use super::driver::{Driver, Faults};
use crate::clock::Clock;
use backend::SerialPorts;

/// Credit error: the selector and the I/O board disagree, or the selector does not answer.
const ERR_COIN_COUNT_OR_COMM: u16 = 3991;
/// Credit error: the selector reports a busy or hardware failure.
const ERR_BUSY_OR_HARDWARE: u16 = 3992;
/// Credit error: the I/O board reports the coin counter disconnected.
const ERR_COUNTER_DISCONNECTED: u16 = 3993;
/// Credit error: the selector keeps failing its setup.
const ERR_SETUP_FAILING: u16 = 4105;
/// Frames the requested lockout state is shown before the device's own state wins.
const LOCKOUT_COOLDOWN: u32 = 60;
/// Frames the setup-fail history keeps.
const RING_LEN: usize = 600;
/// Consecutive mismatching frames that raise the coin count alarm.
const MISMATCH_LIMIT: u32 = 180;
/// Counter slots the selector cross-checks.
const SLOTS: usize = 2;

/// The condition of one coin slot of the I/O board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotCondition {
    /// Counting normally.
    Ok,
    /// The coin chute is jammed.
    Jam,
    /// The counter is disconnected.
    Disconnected,
}

/// The coin node the selector's pulses are wired to (`updateInputInfo`).
pub trait InputNode {
    /// Number of coin slots of the node.
    fn slot_count(&self) -> usize;
    /// Condition of slot `slot`.
    fn condition(&self, slot: usize) -> SlotCondition;
    /// The 16-bit coin counter of slot `slot`.
    fn counter(&self, slot: usize) -> u16;
}

/// What the device needs from the world for one frame.
pub struct Io<'a> {
    /// The cabinet's COM ports.
    pub ports: &'a dyn SerialPorts,
    /// The clock.
    pub clock: &'a dyn Clock,
    /// The node of the first mapped coin box, when the node manager is active.
    pub node: Option<&'a dyn InputNode>,
}

/// Sample ring with consecutive-run and percentage alarms (`0x140292740` and siblings).
#[derive(Debug, Clone)]
struct Ring {
    limit: u32,
    percent: u32,
    samples: Vec<bool>,
    index: usize,
    run: u32,
}

impl Ring {
    fn new(limit: u32, percent: u32) -> Self {
        Self { limit, percent, samples: vec![false; RING_LEN], index: 0, run: 0 }
    }

    fn clear(&mut self) {
        self.samples.fill(false);
        self.index = 0;
        self.run = 0;
    }

    fn alarmed(&self) -> bool {
        let ones = self.samples.iter().filter(|&&sample| sample).count() as u32;
        (self.limit != 0 && self.limit <= self.run)
            || (self.percent != 0 && (RING_LEN as u32) * self.percent / 100 <= ones)
    }

    /// Pushes a sample; a ring that is in alarm is frozen. Returns whether the ring is in alarm afterwards.
    fn push(&mut self, sample: bool) -> bool {
        if !self.alarmed() {
            self.samples[self.index] = sample;
            self.index = (self.index + 1) % RING_LEN;
            self.run = if sample { self.run + 1 } else { 0 };
        }
        self.alarmed()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Fault {
    current: bool,
    changed: bool,
}

#[derive(Debug, Clone)]
struct Slot {
    accumulated: u32,
    initialised: bool,
    ring: Ring,
}

impl Slot {
    fn new() -> Self {
        Self { accumulated: 0, initialised: false, ring: Ring::new(MISMATCH_LIMIT, 0) }
    }
}

/// The initial value of the driver's seed generator (`0x140404910`): the performance counter, retried until
/// neither its low 32 bits nor its low 16 bits are zero.
pub fn initial_seed(clock: &dyn Clock) -> u32 {
    for _ in 0..64 {
        if let Some(counter) = clock.performance_counter() {
            let low = counter as u32;
            if low != 0 && low as u16 != 0 {
                return low;
            }
        }
    }
    0xDEAD_BEEF
}

/// The AS-6DB coin selector as the credit manager sees it.
#[derive(Debug)]
pub struct SpecialDevice {
    config: CreditAs6db,
    started: bool,
    driver: Option<Driver>,
    cooldown: u32,
    lockout: bool,
    faults: [Fault; 10],
    setup_ring: Ring,
    slots: [Slot; SLOTS],
    error: Option<u16>,
}

impl Default for SpecialDevice {
    fn default() -> Self {
        Self {
            config: CreditAs6db::default(),
            started: false,
            driver: None,
            cooldown: 0,
            lockout: false,
            faults: [Fault::default(); 10],
            setup_ring: Ring::new(0, 1),
            slots: [Slot::new(), Slot::new()],
            error: None,
        }
    }
}

impl SpecialDevice {
    /// `0x1402637D0`: takes the config block; an enabled block starts the driver (no port is opened yet).
    pub fn set_config(&mut self, config: &CreditAs6db, clock: &dyn Clock) {
        self.config = config.clone();
        if self.config.enable {
            self.init_driver(clock);
        }
        self.started = true;
    }

    /// `0x140263B20`: a fresh driver; the device boots locked out.
    fn init_driver(&mut self, clock: &dyn Clock) {
        let port = u16::try_from(self.config.arg).unwrap_or(0);
        let now = clock.tick_ms() as u32;
        self.driver = Some(Driver::new(port, self.config.types.as_slice(), now, initial_seed(clock)));
        self.lockout = true;
    }

    /// `0x140263800`: closes the driver and forgets all state.
    pub fn shutdown(&mut self) {
        if self.started {
            self.driver = None;
            self.reset();
            self.started = false;
        }
    }

    /// `0x1402639D0`: forgets the error, the fault edges, the slot baselines and the cooldown; the driver and
    /// the lockout state stay.
    pub fn reset(&mut self) {
        self.error = None;
        self.faults = [Fault::default(); 10];
        for slot in &mut self.slots {
            *slot = Slot::new();
        }
        self.cooldown = 0;
        self.setup_ring.clear();
    }

    /// `isAvailable()`: the config block's enable byte.
    pub fn is_available(&self) -> bool {
        self.config.enable
    }

    /// The error record's code, if the device raised one.
    pub fn error(&self) -> Option<u16> {
        self.error
    }

    /// `S[316]`: the lockout state shown to the game.
    pub fn lockout_on(&self) -> bool {
        self.lockout
    }

    fn any_fault(&self) -> bool {
        self.faults.iter().any(|fault| fault.current)
    }

    /// `0x140263990`.
    pub fn can_lockout(&self) -> bool {
        self.driver.is_some() && self.cooldown == 0 && !self.any_fault()
    }

    /// `0x140263910`: `false` when the device cannot change its lockout now.
    pub fn special_lockout(&mut self, on: bool) -> bool {
        if !self.can_lockout() {
            return false;
        }
        if on != self.lockout {
            if let Some(driver) = self.driver.as_mut() {
                driver.set_lockout(on);
            }
            self.cooldown = LOCKOUT_COOLDOWN;
            self.lockout = on;
        }
        true
    }

    /// `0x140263850`: one libam frame.
    pub fn execute(&mut self, io: &mut Io<'_>) {
        if !self.started {
            return;
        }
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        driver.frame(io.ports, io.clock.tick_ms() as u32);
        self.housekeeping();
        self.update_error_info(io);
        self.update_input_info(io);
    }

    /// `0x140264300`: the requested lockout is shown until the device confirms it or the cooldown runs out.
    fn housekeeping(&mut self) {
        let Some(driver) = self.driver.as_ref() else {
            return;
        };
        let device = driver.device_lockout();
        if self.cooldown != 0 {
            if device == self.lockout {
                self.cooldown = 0;
            } else {
                self.cooldown -= 1;
            }
        }
        if self.cooldown == 0 {
            self.lockout = device;
        }
    }

    fn raise_if_empty(&mut self, code: u16) {
        if self.error.is_none() {
            self.error = Some(code);
        }
    }

    /// `0x140263CE0`: reacts to the rising edge of each fault condition.
    fn update_error_info(&mut self, io: &mut Io<'_>) {
        let Some(driver) = self.driver.as_ref() else {
            return;
        };
        let Faults {
            comm_timeout,
            busy_timeout,
            rejected,
            hardware_fail,
            setup_fail,
            reject_fail,
            accept_coin_fail,
            accept_stocked_coin_fail,
            coin_jam,
            reverse,
        } = driver.faults();
        let current = [
            comm_timeout,
            busy_timeout,
            hardware_fail,
            setup_fail,
            reject_fail,
            accept_coin_fail,
            coin_jam,
            rejected,
            accept_stocked_coin_fail,
            reverse,
        ];
        let mut rising = [false; 10];
        for (k, fault) in self.faults.iter_mut().enumerate() {
            let changed = fault.current != current[k];
            fault.current = current[k];
            fault.changed = changed;
            rising[k] = changed && current[k];
        }
        if rising[0] {
            tracing::error!("as-6db: communication timeout");
            self.raise_if_empty(ERR_COIN_COUNT_OR_COMM);
        }
        if rising[1] {
            tracing::error!("as-6db: busy timeout");
            self.raise_if_empty(ERR_BUSY_OR_HARDWARE);
        }
        if rising[2] {
            tracing::error!("as-6db: hardware fail");
            self.raise_if_empty(ERR_BUSY_OR_HARDWARE);
        }
        if rising[3] {
            tracing::warn!("as-6db: setup fail");
            self.init_driver(io.clock);
        }
        if rising[4] {
            tracing::warn!("as-6db: reject fail");
        }
        if rising[5] {
            tracing::warn!("as-6db: accept coin fail");
        }
        if rising[6] {
            tracing::warn!("as-6db: coin jam");
        }
        if rising[7] {
            tracing::warn!("as-6db: coin rejected");
        }
        if rising[8] {
            tracing::warn!("as-6db: accept stocked coin fail");
        }
        if rising[9] {
            tracing::warn!("as-6db: coin reverse");
        }
        if self.setup_ring.push(current[3]) {
            self.raise_if_empty(ERR_SETUP_FAILING);
        }
    }

    /// `0x140264360`: the selector's counted coins against the I/O board's coin counters.
    fn update_input_info(&mut self, io: &mut Io<'_>) {
        let (Some(node), Some(driver)) = (io.node, self.driver.as_ref()) else {
            return;
        };
        let stats = driver.stats();
        let types = self.config.types.as_slice();
        for index in 0..node.slot_count().min(SLOTS) {
            match node.condition(index) {
                SlotCondition::Jam => {
                    tracing::warn!("as-6db: coin jam detected by the i/o board");
                    return;
                }
                SlotCondition::Disconnected => {
                    tracing::error!("as-6db: coin counter disconnected");
                    self.error = Some(ERR_COUNTER_DISCONNECTED);
                    return;
                }
                SlotCondition::Ok => {}
            }
            let delta = match types.get(index) {
                Some(1) => stats.yen_10,
                Some(2) => stats.yen_100,
                Some(3) => stats.yen_500,
                _ => 0,
            };
            let counter = node.counter(index);
            let slot = &mut self.slots[index];
            slot.accumulated = slot.accumulated.wrapping_add(delta);
            if !slot.initialised {
                slot.initialised = true;
                slot.accumulated = u32::from(counter);
            }
            let mismatch = (slot.accumulated ^ u32::from(counter)) & 0xF != 0;
            if slot.ring.push(mismatch) {
                tracing::error!(selector = slot.accumulated, io_board = counter, "as-6db: coin counts do not match");
                self.error = Some(ERR_COIN_COUNT_OR_COMM);
                return;
            }
        }
    }
}
