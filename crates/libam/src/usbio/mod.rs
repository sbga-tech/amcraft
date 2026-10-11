//! USB I/O (`am::usbio::Manager`, `spec/libam/usbio.md` part 1), the system I/O manager (part 3) and the USB
//! device checker (part 4).
//!
//! # Cross-module methods
//!
//! * [`UsbIo::update`] (`0x140251A70`, M.3): runs before the credit step of the same frame, so a coin counter
//!   change is seen by the credit manager in that pass.
//! * [`crate::credit::CoinSlots`] for [`UsbIo`]: the node interface the credit manager reads when the USB I/O
//!   adapter is selected (`usbio.md` 1.2; credit.md "node adapter selection"). Nodes are numbered in the
//!   configured node order. The coin word of a slot is the board's reply word: the counter byte, the jam flag
//!   (bit 0) and the disconnect flag (bit 1). The USB I/O board has no coin lock-out output.
//! * [`SystemIo::dip`]: the DIP word of the `LoadCabinetType` event (M.5).
//! * [`UsbIo::enabled`]: the adapter-selection flag (`ctx+55168`).

mod board;
mod firmware;
mod node;
mod port;
mod reconnect;
pub mod systemio;
pub mod usbdevice;

use std::rc::Rc;

use crate::config::UsbIoConfig;

use crate::credit::CoinSlots;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use board::{BoardDesc, parse_board};
use firmware::{FirmwareFile, parse_firmware};
use node::{Node, error, mode};
pub use port::{
    BoardIoHandle, BoardIoPort, FirmwareEvent, FirmwareStatus, UNIQUE_DATA_LEN, UNIQUE_INPUT_DATA_LEN, UniqueCommand,
    UniqueDescriptor, UniqueDescriptors, UsbDeviceAppConfig, UsbDeviceEvent, UsbDeviceHandle, UsbDeviceId,
    UsbDevicePort, UsbIoAppConfig, UsbIoHandle, UsbIoNodeFunctions, UsbIoNodeInfo, UsbIoPort, UsbIoProcessConfig,
    UsbIoUniqueFunction, UsbRequired,
};
pub use systemio::SystemIo;
pub use usbdevice::UsbDevice;

/// Firmware-session mode (`Manager+0x8F0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Session {
    #[default]
    None,
    Active,
    Finished,
}

/// `am::usbio::Manager` (`ctx+35528`).
#[derive(Debug, Default)]
pub struct UsbIo {
    life: Lifecycle,
    config: UsbIoConfig,
    enabled: bool,
    nodes: Vec<Node>,
    boards: Vec<BoardDesc>,
    dev_state: [u8; 4],
    setup_step: u8,
    test_mode: bool,
    session: Session,
    session_step: u8,
    verification: bool,
    file: Option<Rc<FirmwareFile>>,
    update_active: bool,
    update_index: usize,
    error: ErrorCell,
    events: Vec<FirmwareEvent>,
}

impl UsbIo {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter`: takes `AppConfig.usbio`, state 1 (`setConfig`, `0x1402517C0`). A disabled
    /// configuration goes to state 2 at once, with no node and no DetectNode event.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.usbio.clone();
        self.enabled = self.config.enable;
        self.life.begin();
        self.setup_step = 1;
        if !self.enabled {
            self.life.state = StateWord::READY;
        }
    }

    fn raise(&mut self, code: u16, sub: usize) {
        self.error.set(code, if self.config.append_sub_number_to_device_error { sub as u16 } else { 0 });
    }

    fn emit(&mut self, event: FirmwareEvent) {
        if self.config.app.firmware_listener {
            self.events.push(event);
        }
    }

    /// Lists the boards on the bus; a failure raises 6401 and leaves the list empty.
    fn enumerate(&mut self, cx: &mut Cx<'_>) {
        self.boards = match cx.backend.hid.enumerate() {
            Ok(found) => found.iter().filter_map(parse_board).collect(),
            Err(_) => {
                self.raise(error::NOT_FOUND, 1);
                Vec::new()
            }
        };
    }

    /// `0x140250A60` (`initial`: the node-set init, which compares the firmware file with the board) and
    /// `0x140250DF0` (the session's reinit: mode 3 for a board that needed an update, unless this is the
    /// verification pass): matches every configured node to the first board of its id.
    fn bind_nodes(&mut self, cx: &mut Cx<'_>, initial: bool) {
        let previous = self.dev_state;
        for (index, &was) in previous.iter().enumerate().take(self.nodes.len()) {
            let id = self.nodes[index].config().id;
            let Some(desc) = self.boards.iter().find(|b| b.config & 0x0F == id).cloned() else {
                self.dev_state[index] = 0;
                self.raise(error::NOT_FOUND, index + 1);
                continue;
            };
            let Ok(dev) = cx.backend.hid.open(&desc.path) else {
                self.dev_state[index] = 0;
                self.nodes[index].raise(error::CONTEXT);
                continue;
            };
            let node_mode = if initial {
                match &self.file {
                    Some(file) if !self.test_mode && file.differs_from(&desc.board_no, desc.revision) => {
                        mode::UPDATE_NEEDED
                    }
                    _ => mode::NORMAL,
                }
            } else if was == 2 && !self.verification {
                mode::FIRMWARE
            } else {
                mode::NORMAL
            };
            self.dev_state[index] = match node_mode {
                mode::UPDATE_NEEDED => 2,
                mode::FIRMWARE => 3,
                _ => 1,
            };
            self.nodes[index].initialize(dev, &desc, node_mode, true);
        }
    }

    /// `0x140250A60`: builds the nodes and opens the firmware file.
    fn node_set_init(&mut self, cx: &mut Cx<'_>) {
        let append = self.config.append_sub_number_to_device_error;
        self.nodes =
            self.config.nodes.iter().enumerate().map(|(index, cfg)| Node::new(index, cfg.clone(), append)).collect();
        self.dev_state = [0; 4];
        self.file = None;
        let path = self.config.firmware_path.as_str();
        if !path.is_empty() && !self.test_mode {
            let parsed = std::fs::read(cx.backend.fs.map(path))
                .ok()
                .and_then(|bytes| parse_firmware(path, &String::from_utf8_lossy(&bytes)));
            match parsed {
                Some(file) => self.file = Some(Rc::new(file)),
                None => self.raise(error::FIRMWARE, 1),
            }
        }
        self.bind_nodes(cx, true);
    }

    fn setup_step(&mut self, cx: &mut Cx<'_>, now: u64) {
        match self.setup_step {
            1 => {
                self.enumerate(cx);
                if self.error.get().is_some() {
                    self.life.state = StateWord::READY;
                }
                self.node_set_init(cx);
                self.setup_step = 2;
            }
            _ => {
                for node in &mut self.nodes {
                    node.update(now);
                }
                if self.nodes.iter().all(Node::is_settled) {
                    self.life.state = StateWord::READY;
                    if self.nodes.iter().any(|n| n.mode == mode::UPDATE_NEEDED) {
                        self.session = Session::Active;
                        self.session_step = 1;
                        self.verification = false;
                    }
                }
            }
        }
    }

    /// `0x140251390`: starts the next node that has a board.
    fn start_next_update(&mut self) {
        while self.update_index < self.nodes.len() && self.dev_state[self.update_index] == 0 {
            self.update_index += 1;
        }
        let index = self.update_index;
        if index >= self.nodes.len() {
            self.update_active = false;
            return;
        }
        let file = self.file.clone();
        self.nodes[index].update_firmware(file);
        self.emit(FirmwareEvent::NodeStatus { node: index as u64, status: FirmwareStatus::Started });
    }

    /// `0x140251FA0`: one step of the firmware session.
    fn session_tick(&mut self, cx: &mut Cx<'_>, now: u64) {
        match self.session_step {
            1 => {
                for node in &mut self.nodes {
                    node.finalize(now);
                }
                if !self.verification {
                    self.emit(FirmwareEvent::UpdateState { status: FirmwareStatus::Started });
                }
                self.session_step = 2;
            }
            2 => {
                self.enumerate(cx);
                self.bind_nodes(cx, false);
                self.session_step = 3;
            }
            3 => {
                if !self.nodes.iter().all(Node::is_settled) {
                    return;
                }
                if self.verification {
                    self.emit(FirmwareEvent::UpdateState { status: FirmwareStatus::Finished });
                    self.session = Session::Finished;
                } else {
                    self.update_active = true;
                    self.update_index = 0;
                    self.start_next_update();
                    self.session_step = 4;
                }
            }
            _ => {
                let index = self.update_index;
                if self.update_active && index < self.nodes.len() {
                    let percent = self.nodes[index].progress();
                    self.emit(FirmwareEvent::Progress { node: index as u64, percent });
                    if self.nodes[index].is_update_finished() {
                        self.emit(FirmwareEvent::NodeStatus { node: index as u64, status: FirmwareStatus::Finished });
                        self.update_index += 1;
                        self.start_next_update();
                    }
                }
                if !self.update_active {
                    self.verification = true;
                    self.session_step = 1;
                }
            }
        }
    }

    /// `0x140251A70`, once per Operating frame, after CAN (M.3).
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        let now = cx.clock.tick_ms();
        if self.life.state == StateWord::READY {
            for node in &mut self.nodes {
                node.update(now);
            }
            if self.session == Session::Active {
                self.session_tick(cx, now);
            }
        } else if self.enabled {
            self.setup_step(cx, now);
            if self.life.state == StateWord::READY && self.config.detect_node_listener {
                cx.events.push(crate::interface::LibamEvent::UsbIoDetectNode);
            }
        }
        self.publish_error();
    }

    /// The manager's error cell reports its own record, or else the first node's.
    fn publish_error(&mut self) {
        self.life.error = if self.error.is_empty() {
            self.nodes.iter().map(|n| n.error).find(|e| !e.is_empty()).unwrap_or(ErrorCell::EMPTY)
        } else {
            self.error
        };
    }

    /// `0x1402518B0`: zero outputs go out in one last tick, the boards are released, state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let now = cx.clock.tick_ms();
        for node in &mut self.nodes {
            node.reset_outputs(true);
            node.update(now);
        }
        cx.clock.sleep_ms(1);
        for node in &mut self.nodes {
            node.finalize(now);
        }
        if self.session == Session::Active && (self.session_step >= 2 || self.verification) {
            self.emit(FirmwareEvent::UpdateState { status: FirmwareStatus::Finished });
        }
        let events = std::mem::take(&mut self.events);
        *self = Self::new();
        self.events = events;
    }

    /// Module restart (`0x140251B00`). `now` is the tick the nodes are released at.
    ///
    /// A ready, enabled manager with an error, a firmware session or a firmware file (outside the test side)
    /// releases the firmware file and every node, drops the board list, the error and the session, tells the
    /// listener the session ended when one was past its start, and runs the set-up again from state 1. Any other
    /// manager only zeroes the outputs of its nodes.
    pub fn restart(&mut self, test_mode: bool, now: u64) {
        self.test_mode = test_mode;
        if self.life.state != StateWord::READY || !self.enabled {
            return;
        }
        let failed = !self.error.is_empty() || self.nodes.iter().any(|n| !n.error.is_empty());
        let firmware = !self.config.firmware_path.as_str().is_empty() && !test_mode;
        if failed || self.session == Session::Active || firmware {
            let session_started = self.session == Session::Active
                && self.session_step != 0
                && (self.session_step != 1 || self.verification);
            self.file = None;
            for node in &mut self.nodes {
                node.finalize(now);
            }
            self.nodes.clear();
            self.boards.clear();
            self.update_active = false;
            self.update_index = 0;
            self.error.clear();
            self.session = Session::None;
            self.verification = false;
            if session_started {
                self.emit(FirmwareEvent::UpdateState { status: FirmwareStatus::Finished });
            }
            self.life.begin();
            self.setup_step = 1;
        } else {
            for node in &mut self.nodes {
                node.reset_outputs(false);
            }
        }
        self.publish_error();
    }

    /// `allReady` term: state 2 (the manager is ready or disabled).
    pub fn is_ready(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// `ctx+37508`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// The error cell scanned by the aggregator.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// `usbio.enable`.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The configuration taken at initialize.
    pub fn config(&self) -> &UsbIoConfig {
        &self.config
    }

    pub(crate) fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub(crate) fn nodes_mut(&mut self) -> &mut [Node] {
        &mut self.nodes
    }

    /// Listener calls queued since the last drain.
    pub(crate) fn take_firmware_events(&mut self) -> Vec<FirmwareEvent> {
        std::mem::take(&mut self.events)
    }
}

impl CoinSlots for UsbIo {
    fn enabled(&self) -> bool {
        self.enabled
    }

    /// State 2 of an enabled manager: the nodes can be mapped.
    fn ready(&self) -> bool {
        self.enabled && self.life.state == StateWord::READY
    }

    /// The configured node count, whichever boards exist (`usbio.md` 1.3.3).
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The coin slot count of the node (`Node+0x310`, at most 2).
    fn slot_count(&self, node: usize) -> usize {
        self.nodes.get(node).map_or(0, |n| n.coin.count.min(2) as usize)
    }

    /// `share_coin_slot` of a node's configuration (`0x14024E8B0`).
    fn share_coin_slot(&self, node: usize) -> bool {
        self.nodes.get(node).is_some_and(|n| n.config().share_coin_slot)
    }

    /// Bit 0 of the slot's flag byte.
    fn jam(&self, node: usize, slot: usize) -> bool {
        self.coin_word(node, slot).is_some_and(|word| word.flags & 1 != 0)
    }

    /// Bit 1 of the slot's flag byte.
    fn disconnected(&self, node: usize, slot: usize) -> bool {
        self.coin_word(node, slot).is_some_and(|word| word.flags & 2 != 0)
    }

    /// The 8-bit counter byte of the slot's reply word.
    fn coin_counter(&self, node: usize, slot: usize) -> u16 {
        self.coin_word(node, slot).map_or(0, |word| word.code & 0xFF)
    }

    /// The configured player count of the node (0..4).
    fn switch_count(&self, node: usize) -> usize {
        self.nodes.get(node).map_or(0, |n| n.switch.player_count as usize)
    }

    /// Service switch (`0x40`) of player `k` of a node.
    fn service(&self, node: usize, k: usize) -> bool {
        self.nodes
            .get(node)
            .is_some_and(|n| k < n.switch.player_count.min(4) as usize && n.switch.player_masks[k] & 0x40 != 0)
    }

    /// The USB I/O board has no coin lock-out output: libam's lock-out reaches only the AS-6DB selector.
    fn set_lockout(&mut self, _node: usize, _on: bool) {}
}

impl UsbIo {
    /// The reply word of coin slot `slot` of `node`, when the node has that slot.
    fn coin_word(&self, node: usize, slot: usize) -> Option<UniqueDescriptor> {
        let node = self.nodes.get(node)?;
        (slot < node.coin.count.min(2) as usize).then(|| node.coin.entries[slot])
    }
}
