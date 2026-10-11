//! JVS (`am::jvs::Manager`, `spec/libam/jvs.md`): the JVS bus, its nodes and the input/output ports.
//!
//! # The disabled-feature build
//!
//! The libam linked into this image is the *disabled-feature* variant [jvs.variant.disabled_manager]: there is no
//! bus, no serial port, no framing and no node enumeration [jvs.variant.no_device_io]. The node list is never
//! populated [jvs.nodes.never_populated], so this module keeps no node storage at all: every node query is a query
//! of an empty list. What remains is:
//!
//! * the state machine (`jvs.md` 3.1): state 2 when `jvs.enable` is off, otherwise state 1 until the first tick,
//!   which runs the "not supported" setup (module error 4104, state 3) and fires `DetectNode` once
//!   [jvs.manager.setup_stub] [jvs.event.detect_node];
//! * the re-arm of an enabled module that holds an error [jvs.manager.reset_on_error];
//! * the facade: [`JvsHandle`] / [`JvsNodeHandle`] report zero nodes and fail every indexed call with result code 1
//!   (`invalid num`) [jvs.facade.node_count] [jvs.facade.auto_output_zero] [jvs.facade.output_slot]
//!   [jvs.facade.node_input];
//! * the credit node-manager adapter ([`CoinSlots`]): never enabled, no counters [jvs.cnm.vtable].
//!
//! # Cross-module methods
//!
//! * [`Jvs::update`] (`0x140246480`, M.3): fires `DetectNode` (`LibamEvent::JvsDetectNode`) through `cx.events`.
//! * [`crate::credit::CoinSlots`] for [`Jvs`]: `enabled()` is the adapter's `enabledFlag`, which this image never
//!   sets, so the flow's `CoinAdapter::select` never picks JVS; `coin_counter` is always `None` and `set_lockout`
//!   is a no-op (the AS-6DB lock-out belongs to the credit module, not to JVS nodes).
//! * [`Jvs::restart`]: the module sweep's re-arm (`0x1402464F0`); the sweep's own gate (force, or the error's reset
//!   type 2/4) is evaluated by the caller.

mod port;
#[cfg(test)]
mod tests;

use crate::config::JvsConfig;

use crate::credit::CoinSlots;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};

pub(crate) use port::clamp_len;
pub use port::{
    AnalogChannel, AnalogState, CONFIG_BLOCKS, InputConfig, InputDevice, InputPort, InputRelation, JvsHandle,
    JvsNodeHandle, JvsNodeInput, JvsNodePort, JvsPort, MAX_JVS_NODES, MAX_RELATIONS, OutputConfig, OutputDevice,
    OutputPort, OutputRelation, PLAYER_SETS, RelationBlock, RelationMap, RelationSet, RotaryState, SwitchState,
    TooManyRelations,
};

/// The module error number of the "not supported" setup; also the generic "configuration invalid" number
/// [jvs.error.invalid_config_same_code].
const NOT_SUPPORTED_ERROR: u16 = 4104;

/// `am::jvs::Manager` (`ctx+26640`).
#[derive(Debug, Default)]
pub struct Jvs {
    life: Lifecycle,
    config: JvsConfig,
}

impl Jvs {
    /// The constructed manager (state 0, default config).
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`0x140246330` apply config): copies `AppConfig.jvs`, state 1 when `enabled` else 2
    /// [jvs.manager.apply_config].
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.jvs.clone();
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x140246480`, once per Operating frame, after the platform and systemio [jvs.manager.tick].
    ///
    /// A module that is not yet done logs that JVS is unsupported, sets error 4104 and state 3, then fires
    /// `DetectNode` when the daemon installed its listener. A disabled module (state 2) does nothing.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if self.life.state.is_done() {
            return;
        }
        tracing::error!("jvs is not supported by this libam build");
        self.life.error.set(NOT_SUPPORTED_ERROR, 0);
        self.life.state = StateWord::FAILED;
        if self.config.detect_node_listener {
            cx.events.push(crate::interface::LibamEvent::JvsDetectNode);
        }
    }

    /// `0x140246410` (shutdown): back to state 0 with the config restored to its defaults [jvs.manager.stop].
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if self.life.state != StateWord::UNINITIALIZED {
            self.config = JvsConfig::default();
        }
        self.life.end();
    }

    /// Re-arm (`0x1402464F0`): a finished, enabled module that holds an error clears it and goes back to state 1,
    /// so the next tick runs the setup again (same error, `DetectNode` fires again) [jvs.manager.reset_on_error].
    pub fn restart(&mut self) {
        if self.life.state.is_done() && self.config.enable && !self.life.error.is_empty() {
            self.life.error.clear();
            self.life.state = StateWord::INITIALIZED;
        }
    }

    /// `ctx+28408`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term (`0x14029DFB0`): state in {2, 3} [jvs.manager.all_ready_gate].
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`ctx+32208`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

impl CoinSlots for Jvs {
    /// The adapter's `enabledFlag` (`ctx+32512`): its only setter has no caller, so it is never set
    /// [jvs.cnm.selection] [jvs.manager.ready_flag].
    fn enabled(&self) -> bool {
        false
    }

    /// The node list is never populated [jvs.nodes.never_populated]: no node, slot, switch or output exists.
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
