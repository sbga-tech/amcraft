//! CAN (`am::can::Manager`, `spec/libam/can.md` section 2): the CAN boards and their nodes.
//!
//! # The disabled-feature build
//!
//! The CAN manager linked into this image only reports "not supported" [can.manager.disabled_variant]. There is no
//! framing, no driver, no worker and no bit rate; the port count is the constant 0 for the life of the process
//! [can.manager.port_count_constant_zero], so this module keeps no port or node storage:
//!
//! * `setup()` (run by the first update after the config was applied, and again after every restart that finds
//!   an error) logs that CAN is not supported, raises module error 4104 and moves to state 2
//!   [can.manager.state_machine] [can.manager.not_supported_error];
//! * the facade ([`CanHandle`]) reports zero ports and zero targets, fails `target` and `send` with result code 1
//!   and never raises a send-complete or received-packet event [can.facade.port_count] [can.facade.target_count]
//!   [can.facade.target] [can.facade.send] [can.listener.never_dispatched];
//! * the credit node-manager sub-object is never active [can.node_manager.inactive].
//!
//! The configuration validation of `can.md` 2.6 lives in [`AmConfig::validate`](crate::config::AmConfig::validate),
//! which runs before this module sees the config.
//!
//! # Cross-module methods
//!
//! * [`Can::update`] (`0x140245C20`, M.3).
//! * [`crate::credit::CoinSlots`] for [`Can`]: `enabled()` is the adapter's `isActive` byte, which is never
//!   written, so the flow's `CoinAdapter::select` never picks CAN; no counters, lock-out is a no-op.
//! * [`Can::restart`]: the module restart gate (`0x140245C60`); the sweep's own gate (force, or the error's reset
//!   type 2/4) is evaluated by the caller.

mod port;
#[cfg(test)]
mod tests;

use crate::config::CanConfig;

use crate::credit::CoinSlots;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};

pub use port::{CanAppConfig, CanEvent, CanHandle, CanPacket, CanPort, CanTargetInfo, DisabledCanPort, MAX_FRAME_DATA};

/// The error number the "not supported" setup raises (`can.md` 2.5).
const NOT_SUPPORTED_ERROR: u16 = 4104;

/// `am::can::Manager` (`ctx+32536`).
#[derive(Debug, Default)]
pub struct Can {
    life: Lifecycle,
    config: CanConfig,
}

impl Can {
    /// The constructed manager (state 0).
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`0x140245B00`): copies `AppConfig.can`, state 1 when `enabled` else 2
    /// (`can.md` 2.5).
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.can.clone();
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x140245C20`, once per Operating frame, right after JVS: `if state != 2 { setup() }`.
    pub fn update(&mut self) {
        if self.life.state != StateWord::READY {
            tracing::error!("can is not supported by this libam build");
            self.life.error.set(NOT_SUPPORTED_ERROR, 0);
            self.life.state = StateWord::READY;
        }
    }

    /// `0x140245BE0` (shutdown): back to state 0 with the config copy reset.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if self.life.state != StateWord::UNINITIALIZED {
            self.config = CanConfig::default();
        }
        self.life.end();
    }

    /// Module restart (`0x140245C60`, unconditional): the state becomes `(portCount != 0 || error != 0) ? 1 : 2`
    /// and the error holder is cleared. The port count is 0, so a module that raised 4104 runs `setup()` again on
    /// the next update and re-raises it after every restart (`can.md` 2.5).
    pub fn restart(&mut self) {
        self.life.state = if self.life.error.is_empty() { StateWord::READY } else { StateWord::INITIALIZED };
        self.life.error.clear();
    }

    /// `ctx+35468`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state == 2 (JVS accepts 2 and 3, CAN exactly 2).
    pub fn is_ready(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// The error cell (`ctx+35168`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

impl CoinSlots for Can {
    /// The node-manager `isActive` byte (`manager+0xB88+8`): never written (`can.md` 2.2.4).
    fn enabled(&self) -> bool {
        false
    }

    /// No node exists (`can.md` 2.2.4): no slot, switch or output.
    fn ready(&self) -> bool {
        false
    }

    fn node_count(&self) -> usize {
        0
    }

    fn slot_count(&self, _node: usize) -> usize {
        0
    }

    fn share_coin_slot(&self, _node: usize) -> bool {
        false
    }

    fn jam(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn disconnected(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn coin_counter(&self, _node: usize, _slot: usize) -> u16 {
        0
    }

    fn switch_count(&self, _node: usize) -> usize {
        0
    }

    fn service(&self, _node: usize, _k: usize) -> bool {
        false
    }

    fn set_lockout(&mut self, _node: usize, _on: bool) {}
}
