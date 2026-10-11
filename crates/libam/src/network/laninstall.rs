//! LAN install (`am::laninstall::Manager`, `spec/libam/network.md` part L) inside the NetInstall composite
//! (`0x140243F00`), which also holds NetDeliver (part D).
//!
//! A server (mode 2) broadcasts the amsBeacon datagram every `interval` seconds and collects the TCP sync
//! payloads of clients that hold its image; a client (mode 1) listens for that beacon, and on a beacon of a
//! single server with a different foreground image writes the install request into the registry. The original's
//! worker threads and request mailboxes are folded into one per-frame step; their observable results are kept.

use std::net::Ipv4Addr;

use crate::config::LanInstallConfig;
use crate::interface::LibamEvent;

use super::deliver_beacon::BeaconContext;
use super::deliver_icf::Icf;
use super::netdeliver::{DeliverAuthParam, DeliverInputs, NetDeliver};
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::registry_keys::MOUNT_KEY;
use backend::lan::{Datagram, SyncListener};
use backend::{Backend, KeyMode, RegValue};

/// Registry key of the install request (`0x140303370`, L.12).
const INSTALL_KEY: &str = r"System\SEGA\SystemProperty\Install";
/// Registry key of the NIC selection (`0x1402F9350`).
const NIC_KEY: &str = r"System\Sega\SystemProperty\wirelessNetwork";
/// Milliseconds between two attempts to learn the adapter address (the 5 s IP poll).
const IP_POLL_MS: u64 = 5_000;
/// Whole seconds without a valid beacon after which `server_count` becomes 0.
const NO_SERVER_S: u64 = 420;
/// Failed registry writes after which the install request is reported anyway.
const INSTALL_RETRIES: u8 = 5;
/// Datagrams handled per frame.
const DATAGRAMS_PER_FRAME: usize = 16;
/// Error: the module context could not be acquired.
const ERROR_CONTEXT: u16 = 4105;
/// Error: several LAN-install servers, client mode.
const ERROR_MULTIPLE_CLIENT: u16 = 8405;
/// Error: several LAN-install servers, server mode.
const ERROR_MULTIPLE_SERVER: u16 = 8406;

/// What the composite reads from other modules every frame.
#[derive(Debug, Clone, Default)]
pub struct NetInputs {
    /// Values NetDeliver and the beacon identity need.
    pub deliver: DeliverInputs,
    /// The ALL.Net `Param` (NetDeliver, non-local mode).
    pub auth: DeliverAuthParam,
    /// `property available` of the active network manager (the latch of L.3.3).
    pub network_property: bool,
}

/// The client role (`amsGcatcher`).
#[derive(Debug)]
struct Catcher {
    socket: Option<Box<dyn Datagram>>,
    ctx: Option<BeaconContext>,
    prev_list: Vec<Ipv4Addr>,
    prev_sender: Option<Ipv4Addr>,
    own_ip: Option<Ipv4Addr>,
    next_open_ms: u64,
    failed: bool,
    installing: bool,
    retries: u8,
    done: bool,
}

/// The server role (`amGdeliverSelf`).
#[derive(Debug)]
struct Server {
    socket: Option<Box<dyn Datagram>>,
    listener: Option<Box<dyn SyncListener>>,
    ctx: Option<BeaconContext>,
    ip: Option<Ipv4Addr>,
    next_open_ms: u64,
    last_send_ms: Option<u64>,
    failed: bool,
}

/// The adapter address when the link is up: `Ok(None)` retry later, `Err` fatal.
fn adapter_ip(backend: &mut Backend, wireless: bool) -> Result<Option<Ipv4Addr>, ()> {
    match super::engine::lan_adapter(&*backend.adapters, wireless) {
        None => Err(()),
        Some((_, false)) => Ok(None),
        Some((ip, true)) => Ok(Some(ip)),
    }
}

/// The NetInstall composite (`ctx+159376`): LAN install and NetDeliver.
#[derive(Debug, Default)]
pub struct NetInstall {
    life: Lifecycle,
    config: LanInstallConfig,
    /// The NetDeliver manager of the composite.
    pub deliver: NetDeliver,
    /// The LAN-install manager state (`LI+32`).
    lan_state: StateWord,
    lan_error: ErrorCell,
    wireless: bool,
    latched: bool,
    permitted: bool,
    clock_start_ms: u64,
    catcher: Option<Catcher>,
    server: Option<Server>,
    /// `IsUpdate.state` (0 unknown, 1 server known, 2 update available).
    is_update: u8,
    multiple: bool,
    server_count: i32,
    /// A server in the installing state (exit reason 2) was heard.
    lan_download_ui: bool,
}

impl Catcher {
    fn new() -> Self {
        Self {
            socket: None,
            ctx: None,
            prev_list: Vec::new(),
            prev_sender: None,
            own_ip: None,
            next_open_ms: 0,
            failed: false,
            installing: false,
            retries: 0,
            done: false,
        }
    }

    /// `0x1402E5AD0`: a second server is a sender outside the previous list that differs from the previous sender.
    fn is_multiple(previous: Option<Ipv4Addr>, list: &[Ipv4Addr], sender: Option<Ipv4Addr>) -> bool {
        let (Some(previous), Some(sender)) = (previous, sender) else { return false };
        !list.is_empty() && !list.contains(&sender) && sender != previous
    }
}

impl Server {
    fn new() -> Self {
        Self { socket: None, listener: None, ctx: None, ip: None, next_open_ms: 0, last_send_ms: None, failed: false }
    }
}

impl NetInstall {
    /// The constructed composite.
    pub fn new() -> Self {
        Self { server_count: -1, ..Self::default() }
    }

    /// `OperatingState::enter`: takes `AppConfig.laninstall` and `netdeliver`, state 1.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.laninstall;
        self.life.begin();
        self.reset_lan();
        self.deliver.initialize(cx);
    }

    /// The part of restart and initialize that returns the LAN manager to "initialised".
    fn reset_lan(&mut self) {
        self.server_count = -1;
        self.is_update = 0;
        self.multiple = false;
        self.lan_error.clear();
        self.latched = false;
        self.permitted = false;
        self.lan_download_ui = false;
        self.catcher = None;
        self.server = None;
        self.lan_state = if self.config.mode == 0 { StateWord::READY } else { StateWord::INITIALIZED };
    }

    /// `0x140244130`, once per Operating frame; the NetDeliver manager runs inside it.
    pub fn update(&mut self, cx: &mut Cx<'_>, active_net_ready: bool, inputs: &NetInputs) {
        if self.life.state == StateWord::INITIALIZED && active_net_ready {
            self.life.finish_setup();
        }
        if self.life.state.is_done() {
            let now_ms = cx.clock.tick_ms();
            if self.config.mode != 0 {
                self.execute(cx, now_ms, inputs);
            }
            self.emit_need_to_exit(cx);
        }
        let done = self.life.state.is_done();
        self.deliver.update(cx, done, &inputs.deliver, &inputs.auth);
        if self.life.error.is_empty() {
            let deliver_error = self.deliver.error_cell();
            if let Some((number, sub)) = deliver_error.get() {
                self.life.error.set(number, sub);
            }
        }
    }

    /// `0x140241410`: the LAN-install manager's execute.
    fn execute(&mut self, cx: &mut Cx<'_>, now_ms: u64, inputs: &NetInputs) {
        if !self.lan_state.is_done() {
            if self.acquire(cx.backend) {
                self.clock_start_ms = now_ms;
                self.lan_state = StateWord::READY;
            } else {
                self.lan_error.set(ERROR_CONTEXT, 0);
                self.lan_state = StateWord::FAILED;
            }
        } else if !self.latched && self.latch_possible() && inputs.network_property {
            self.permitted = inputs.deliver.lan_available;
            self.latched = true;
        }
        if self.lan_state == StateWord::READY || self.lan_state == StateWord::FAILED {
            self.worker_step(cx.backend, now_ms, &inputs.deliver);
        }
        if self.multiple && self.lan_error.is_empty() {
            self.lan_error.set(if self.config.mode == 2 { ERROR_MULTIPLE_SERVER } else { ERROR_MULTIPLE_CLIENT }, 0);
        }
        if let Some((number, sub)) = self.lan_error.get() {
            self.life.error.set(number, sub);
        }
        self.kick();
    }

    /// The module context init (`0x14028FF50`): the NIC selection must find an adapter.
    fn acquire(&mut self, backend: &mut Backend) -> bool {
        let Ok(RegValue::Dword(main_nic)) = backend.registry.read(NIC_KEY, KeyMode::CreateVolatile, "main_nic") else {
            return false;
        };
        self.wireless = main_nic != 0;
        super::engine::lan_adapter(&*backend.adapters, self.wireless).is_some()
    }

    /// L.3.3: the latch runs while the role is neither running nor requested.
    fn latch_possible(&self) -> bool {
        (self.config.mode == 2 && self.server.is_none()) || self.catcher.is_none()
    }

    /// L.3.4: starts the roles once permitted, unless an error is latched or NetDeliver has not settled.
    fn kick(&mut self) {
        let settled = !self.deliver_enabled() || self.deliver.settled();
        if self.config.mode == 0
            || !self.life.error.is_empty()
            || !self.life.state.is_done()
            || !self.permitted
            || self.running()
            || !settled
        {
            return;
        }
        if self.catcher.is_none() {
            self.catcher = Some(Catcher::new());
        }
        if self.config.mode == 2 && self.server.is_none() {
            self.server = Some(Server::new());
        }
    }

    fn deliver_enabled(&self) -> bool {
        self.deliver.is_available()
    }

    /// `0x140241570`: the roles run.
    fn running(&self) -> bool {
        self.lan_state.is_done()
            && self.life.error.is_empty()
            && self.permitted
            && self.catcher.is_some()
            && (self.config.mode != 2 || self.server.is_some())
    }

    /// `0x1402409A0` (L.4): both roles, then `server_count`.
    fn worker_step(&mut self, backend: &mut Backend, now_ms: u64, inputs: &DeliverInputs) {
        self.catcher_step(backend, now_ms, inputs);
        if self.config.mode == 2 {
            self.server_step(backend, now_ms, inputs);
        }
        self.server_count = self.compute_server_count(now_ms);
    }

    /// `0x1402410C0` (L.3.5).
    fn compute_server_count(&self, now_ms: u64) -> i32 {
        if self.config.mode == 0 {
            return -1;
        }
        if self.multiple {
            return 2;
        }
        if self.config.mode == 2 || matches!(self.is_update, 1 | 2) {
            1
        } else if now_ms.saturating_sub(self.clock_start_ms) / 1000 > NO_SERVER_S {
            0
        } else {
            -1
        }
    }

    fn catcher_step(&mut self, backend: &mut Backend, now_ms: u64, inputs: &DeliverInputs) {
        let wireless = self.wireless;
        let mode = self.config.mode;
        let Some(catcher) = self.catcher.as_mut() else { return };
        if !self.permitted || catcher.failed || catcher.done {
            return;
        }
        if catcher.socket.is_none() {
            if now_ms < catcher.next_open_ms {
                return;
            }
            match adapter_ip(backend, wireless) {
                Err(()) => catcher.failed = true,
                Ok(None) => catcher.next_open_ms = now_ms + IP_POLL_MS,
                Ok(Some(ip)) => match backend.lan.open_udp(ip, false) {
                    Err(_) => catcher.failed = true,
                    Ok(socket) => {
                        let crc = foreground_crc_of(backend);
                        let mut ctx = BeaconContext::new(inputs.game_id.clone(), inputs.platform_id.clone(), crc);
                        ctx.own_ip = Some(ip);
                        catcher.own_ip = Some(ip);
                        catcher.ctx = Some(ctx);
                        catcher.socket = Some(socket);
                    }
                },
            }
            return;
        }
        if catcher.installing {
            let ips = catcher.ctx.as_ref().map(|ctx| ctx.ips.clone()).unwrap_or_default();
            if write_install(backend, &ips) || catcher.retries >= INSTALL_RETRIES {
                catcher.installing = false;
                catcher.done = true;
                self.is_update = 2;
            } else {
                catcher.retries += 1;
            }
            return;
        }
        for _ in 0..DATAGRAMS_PER_FRAME {
            let (Some(socket), Some(ctx)) = (catcher.socket.as_mut(), catcher.ctx.as_mut()) else { return };
            let Ok(Some(data)) = socket.recv() else { return };
            ctx.check_beacon(&data);
            if !ctx.valid {
                continue;
            }
            let multiple = Catcher::is_multiple(catcher.prev_sender, &catcher.prev_list, ctx.sender_ip);
            catcher.prev_list = ctx.ips.clone();
            catcher.prev_sender = ctx.sender_ip;
            if multiple {
                self.multiple = true;
                catcher.socket = None;
                catcher.failed = true;
                return;
            }
            if mode == 2 {
                self.is_update = 1;
            } else if ctx.need_sync && ctx.server_state == 0 {
                catcher.installing = true;
                return;
            } else {
                if ctx.server_state >= 2 {
                    self.is_update = 2;
                    self.lan_download_ui = true;
                } else {
                    self.is_update = 1;
                }
                if ctx.need_send_sync()
                    && let (Some(server), Some(own)) = (ctx.sender_ip, ctx.own_ip)
                {
                    let _ = backend.lan.send_sync(server, own);
                }
            }
        }
    }

    fn server_step(&mut self, backend: &mut Backend, now_ms: u64, inputs: &DeliverInputs) {
        let wireless = self.wireless;
        let interval_ms = self.config.interval.saturating_mul(1000);
        let Some(server) = self.server.as_mut() else { return };
        if !self.permitted || server.failed {
            return;
        }
        if server.socket.is_none() {
            if now_ms < server.next_open_ms {
                return;
            }
            match adapter_ip(backend, wireless) {
                Err(()) => server.failed = true,
                Ok(None) => server.next_open_ms = now_ms + IP_POLL_MS,
                Ok(Some(ip)) => {
                    let socket = backend.lan.open_udp(ip, true);
                    let listener = backend.lan.listen_sync(ip);
                    match (socket, listener) {
                        (Ok(socket), Ok(listener)) => {
                            let crc = foreground_crc_of(backend);
                            let mut ctx = BeaconContext::new(inputs.game_id.clone(), inputs.platform_id.clone(), crc);
                            ctx.sender_ip = Some(ip);
                            ctx.add_server_ip(ip);
                            server.ctx = Some(ctx);
                            server.ip = Some(ip);
                            server.socket = Some(socket);
                            server.listener = Some(listener);
                        }
                        _ => server.failed = true,
                    }
                }
            }
            return;
        }
        let (Some(socket), Some(listener), Some(ctx), Some(ip)) =
            (server.socket.as_mut(), server.listener.as_mut(), server.ctx.as_mut(), server.ip)
        else {
            return;
        };
        if server.last_send_ms.is_none_or(|last| now_ms.saturating_sub(last) >= interval_ms) {
            if let Some(bytes) = ctx.build_beacon(ip, 0) {
                let _ = socket.send(&bytes);
            }
            server.last_send_ms = Some(now_ms);
        }
        while let Ok(Some(payload)) = listener.accept() {
            ctx.add_server_ip(Ipv4Addr::from(payload));
        }
    }

    /// `0x140244780` (L.12): level-triggered, kind 2 (NetDeliver) before kind 1 (LAN install).
    fn emit_need_to_exit(&mut self, cx: &mut Cx<'_>) {
        if !self.life.error.is_empty() || !cx.config.sequence.need_to_exit_listener {
            return;
        }
        if self.deliver.needs_exit() {
            cx.events.push(LibamEvent::NeedToExit(2));
        }
        if self.running() && self.config.mode == 1 && self.is_update == 2 {
            cx.events.push(LibamEvent::NeedToExit(1));
        }
    }

    /// `0x1402440E0`.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        self.deliver.terminate(cx);
        self.reset_lan();
        self.lan_state = StateWord::UNINITIALIZED;
        self.life.end();
    }

    /// Module restart (`0x1402414D0`).
    pub fn restart(&mut self) {
        if self.life.state != StateWord::UNINITIALIZED {
            self.life.begin();
            self.reset_lan();
        }
        self.deliver.restart();
    }

    /// The download-UI flag (`0x1402442F0`, `core.md` A.6): an image was installed over LAN.
    pub fn download_ui(&self) -> bool {
        self.lan_download_ui || self.deliver.download_ui()
    }

    /// `laninstall.mode`.
    pub fn mode(&self) -> u32 {
        self.config.mode
    }

    /// `server_count()` (`0x1402416D0`): -1 when off or after error 4105.
    pub fn server_count(&self) -> i32 {
        if self.config.mode == 0 || self.lan_error.get().is_some_and(|(number, _)| number == ERROR_CONTEXT) {
            -1
        } else {
            self.server_count
        }
    }

    /// `allReady` term (`0x1402442C0`).
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`0x1402442E0`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

/// The foreground CRC through the backend (registry read needs `&mut`).
fn foreground_crc_of(backend: &mut Backend) -> u32 {
    let Ok(RegValue::Text(mut base)) = backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, "AMFS") else {
        return 0;
    };
    if !base.ends_with('\\') {
        base.push('\\');
    }
    ["ICF1", "ICF2"]
        .iter()
        .find_map(|name| {
            let bytes = std::fs::read(backend.fs.map(&format!("{base}{name}"))).ok()?;
            Icf::decode(&bytes).ok()
        })
        .map_or(0, |icf| icf.foreground_crc())
}

/// `Install\CMD := 2` and `Install\TARGET := the source addresses` (L.12).
fn write_install(backend: &mut Backend, ips: &[Ipv4Addr]) -> bool {
    if ips.is_empty() {
        return false;
    }
    let target = RegValue::MultiText(ips.iter().map(Ipv4Addr::to_string).collect());
    backend.registry.write(INSTALL_KEY, KeyMode::CreateVolatile, "CMD", &RegValue::Dword(2)).is_ok()
        && backend.registry.write(INSTALL_KEY, KeyMode::CreateVolatile, "TARGET", &target).is_ok()
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddrV4, UdpSocket};
    use std::time::Duration;

    use crate::config::AmConfig;

    use super::*;
    use crate::mock::SimBackend;
    use crate::module::EventQueue;
    use backend::lan::StdLanNet;
    use backend::{InterfaceKind, Nic};

    /// A server on 127.0.0.1 sends the amsBeacon datagram: three NUL-terminated records, state 0, itself first
    /// in the `iplist`, and it sends again only after `interval` seconds.
    #[test]
    fn server_sends_the_beacon_bytes_over_loopback() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = receiver.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let (mut backend, sim) = SimBackend::new(dir.path());
        sim.registry.set(NIC_KEY, "main_nic", RegValue::Dword(0));
        sim.adapters.set_adapters(vec![Nic {
            index: 1,
            name: "loopback".into(),
            mac: Some([0x00, 0xD0, 0xF1, 0, 0, 1]),
            kind: InterfaceKind::Other,
            ip: Ipv4Addr::LOCALHOST,
            mask: Ipv4Addr::new(255, 0, 0, 0),
            up: true,
        }]);
        let lan = StdLanNet::new(0, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port), 0);
        backend.lan = Box::new(lan);
        let config = AmConfig { laninstall: LanInstallConfig { mode: 2, interval: 20 }, ..AmConfig::default() };
        let mut events = EventQueue::default();
        let inputs = NetInputs {
            deliver: DeliverInputs {
                game_id: "SDEZ".into(),
                platform_id: "AAV".into(),
                lan_available: true,
                ..DeliverInputs::default()
            },
            network_property: true,
            ..NetInputs::default()
        };
        let mut net = NetInstall::new();
        let nvram = crate::nvram::NvramContext::new();
        let mut cx =
            Cx { backend: &mut backend, clock: &sim.clock, events: &mut events, config: &config, nvram: &nvram };
        net.initialize(&mut cx);
        for _ in 0..6 {
            net.update(&mut cx, true, &inputs);
        }
        let mut buffer = [0u8; 1100];
        let (length, _) = receiver.recv_from(&mut buffer).unwrap();
        assert_eq!(
            &buffer[..length],
            b"beacon=2&version=2&ip=127.0.0.1&hw=AAV&id=SDEZ&state=0\r\n\0foregroundcrc=0\r\n\0iplist=127.0.0.1\r\n\0"
                .as_slice()
        );
        assert_eq!(net.server_count(), 1);
        receiver.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        net.update(&mut cx, true, &inputs);
        assert!(receiver.recv_from(&mut buffer).is_err(), "no second beacon before the interval");
    }
}
