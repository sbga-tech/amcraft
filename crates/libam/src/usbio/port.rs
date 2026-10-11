//! Port handles of the usbio module.

use libam_api::board_io::BoardIoPort;
use libam_api::jvs::{AnalogChannel, AnalogState, RotaryState, SwitchState};
use libam_api::result::{AmError, AmResult};
use libam_api::usb_device::{UsbDeviceEvent, UsbDevicePort};
use libam_api::usb_io::{
    FirmwareEvent, UNIQUE_INPUT_DATA_LEN, UniqueCommand, UniqueDescriptors, UsbIoNodeInfo, UsbIoPort,
};

use crate::shared::Shared;

use super::node::Node;

/// `UsbIoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct UsbIoHandle(pub(crate) Shared);

/// `BoardIoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct BoardIoHandle(pub(crate) Shared);

/// `UsbDevicePort` handle over the shared libam state.
#[derive(Debug)]
pub struct UsbDeviceHandle(pub(crate) Shared);

/// The failure of every indexed accessor: `index >= nodeCount` (`usbio.md` 1.1).
fn out_of_range() -> AmError {
    AmError::with_message(AmError::INVALID_ARGUMENT, "invalid num")
}

/// The failure of a refused `send_*` (`Node::send*` returned false, `usbio.md` 1.6.2).
fn refused() -> AmError {
    AmError::code(AmError::FAILED)
}

impl UsbIoHandle {
    /// Runs `f` on node `node` after the facade accessor check.
    fn node<R>(&self, node: usize, f: impl FnOnce(&Node) -> R) -> AmResult<R> {
        self.0.guard()?;
        self.0.read(|core| core.usbio.nodes().get(node).map(f)).ok_or_else(out_of_range)
    }

    /// Runs a send on node `node`: `Result 1` out of range, `Result 6` when the node refuses.
    fn send(&mut self, node: usize, f: impl FnOnce(&mut Node) -> bool) -> AmResult<()> {
        self.0.guard()?;
        let accepted = self.0.with(|core| core.usbio.nodes_mut().get_mut(node).map(f)).ok_or_else(out_of_range)?;
        if accepted { Ok(()) } else { Err(refused()) }
    }
}

impl UsbIoPort for UsbIoHandle {
    /// `0x1402188F0`: ok while libam is initialised, else result code 2.
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140217B30`: the configured node count.
    fn node_count(&self) -> u64 {
        self.0.read(|core| core.usbio.nodes().len() as u64)
    }

    fn node_auto_output_zero(&self, node: usize) -> AmResult<bool> {
        self.node(node, |n| n.config().auto_output_zero)
    }

    fn node_info(&self, node: usize) -> Option<UsbIoNodeInfo> {
        self.node(node, |n| n.info).ok()
    }

    fn node_switch(&self, node: usize) -> AmResult<SwitchState<2>> {
        self.node(node, |n| SwitchState {
            system: n.switch.system,
            player_count: n.switch.player_count,
            player_masks: [n.switch.player_masks[0], n.switch.player_masks[1]],
        })
    }

    fn node_analog(&self, node: usize) -> AmResult<AnalogState<8>> {
        self.node(node, |n| {
            let mut channels = [AnalogChannel::default(); 8];
            channels.copy_from_slice(&n.analog.channels[..8]);
            AnalogState { count: n.analog.count, channels }
        })
    }

    fn node_rotary(&self, node: usize) -> AmResult<RotaryState<4>> {
        self.node(node, |n| {
            let mut values = [0; 4];
            values.copy_from_slice(&n.rotary.values[..4]);
            RotaryState { count: n.rotary.count, values }
        })
    }

    fn node_unique_descriptors(&self, node: usize) -> AmResult<UniqueDescriptors> {
        self.node(node, |n| n.coin)
    }

    fn node_unique_data(&self, node: usize) -> AmResult<[u8; UNIQUE_INPUT_DATA_LEN]> {
        self.node(node, |n| n.unique)
    }

    fn send_general_output(&mut self, node: usize, bits: u64, size: u64) -> AmResult<()> {
        self.send(node, |n| n.send_general_output(bits, size))
    }

    fn send_pwm_output(&mut self, node: usize, slot: usize, duty: u8) -> AmResult<()> {
        self.send(node, |n| n.send_pwm_output(slot, duty))
    }

    fn send_unique_command(&mut self, node: usize, command: &UniqueCommand<'_>) -> AmResult<()> {
        self.send(node, |n| n.send_unique_command(command.code, command.data))
    }

    fn take_firmware_events(&mut self) -> Vec<FirmwareEvent> {
        self.0.with(|core| core.usbio.take_firmware_events())
    }
}

impl BoardIoPort for BoardIoHandle {
    /// `0x140218CB0`: ok while libam is initialised, whatever the systemio state (3.4.1).
    fn systemio_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn dip(&self) -> u64 {
        self.0.read(|core| core.systemio.dip())
    }

    fn push(&self) -> u64 {
        self.0.read(|core| core.systemio.push())
    }

    fn led(&self) -> u64 {
        self.0.read(|core| core.systemio.led())
    }

    fn set_led(&mut self, value: u64) {
        self.0.with(|core| core.systemio.set_led(value));
    }
}

impl UsbDevicePort for UsbDeviceHandle {
    /// `0x140218D50`: ok while libam is initialised (4.1).
    fn usbdevice_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140217AA0` (4.2): result code 0 or 2 (no message); the instance is not tested here.
    fn usb_reconnect(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()> {
        let accepted = self.0.with(|core| {
            let master_enabled = core.master.is_attached();
            core.usbdevice.reconnect(vendor_id, product_id, master_enabled)
        });
        if accepted { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
    }

    fn take_events(&mut self) -> Vec<UsbDeviceEvent> {
        self.0.with(|core| core.usbdevice.take_events())
    }
}
