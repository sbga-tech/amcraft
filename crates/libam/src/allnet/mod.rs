//! ALL.Net (`am::allnet::Manager` and the ALW context, `spec/libam/allnet.md`): authentication, accounting,
//! Wi-Fi unit and the app status. Accounting lives in [`billing`], the Wi-Fi units in [`wifi`].
//!
//! # Cross-module methods
//!
//! * [`Allnet::update`] (`0x14023C510`, M.3): `network_ready` is the active net module (`allnet::Manager` when
//!   `auth.enable`, else `am::network::Manager`).
//! * [`Allnet::refresh_idle`] (`0x14023D7E0`, called by `0x140254C40`): app status `4 - (idle != 0)`.
//! * [`Allnet::change_app`] / [`Allnet::cancel_credit`] (`0x14023CF20`, `0x14023CD00`): the APM hand-overs of
//!   `changeApp` (`storage.md` 5.4).
//! * [`Allnet::is_good`]: the auth result the network and LAN-install modules look at (network.md L.14).
//! * `allnet.billing`: see [`billing`].
//!
//! # Authentication
//!
//! [`auth::Auth`] runs the alw phases once per frame; the exchanges go through the backend's TLS client
//! ([`backend::tls::TlsProvider`]), DNS ([`backend::Dns`]), the traceroute ([`trace`]) and the keychip
//! ([`services`]).

pub mod abex;
pub mod alauth;
pub mod auth;
pub mod billing;
pub mod client;
mod dlorder;
pub mod exchange;
pub mod netinfo;
mod netstate;
mod port;
pub mod services;
pub mod trace;
pub mod verify;
pub mod wifi;

use std::fmt;

use crate::config::AllnetAuthConfig;
use crate::nvram::GameId;
use crate::result::{AmError, AmResult};

use crate::clock::Clock;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::network::netdeliver::DeliverAuthParam;
use crate::network::{AmNetwork, NetworkProperty, PropertyBuffer};
use crate::platform::keychip::Link;
use netinfo::{NetClass, NetInfo};

pub use billing::AccountingHandle;
pub use port::{AirHandle, AirPort, AirProcessConfig, AuthHandle, AuthLocation, AuthPort, TimeSlot};
pub use wifi::WifiHandle;

use services::KeychipIdentity;

/// `am::allnet::Manager` (`ctx+150952`) with the ALW context (`ctx+150888`).
#[derive(Default)]
pub struct Allnet {
    life: Lifecycle,
    auth_config: AllnetAuthConfig,
    idle: bool,
    /// `LM+16`: the alw context is acquired.
    context: bool,
    /// `LM+1204` (before the readiness mask of the getter).
    started: bool,
    /// `LM+1205`.
    setup_done: bool,
    /// State 3 of the module (`LM+1168`): the per-frame body runs; state 1 and 2 are the set-up pass.
    pumping: bool,
    app_status: u32,
    apm_game_id: Option<String>,
    apm_version: Option<String>,
    identity: Option<KeychipIdentity>,
    netinfo: NetInfo,
    lan_modes: bool,
    /// `location_router_directly` (the manager's config copy at `+1024`): 0, 1 or 2.
    location_mode: u32,
    deliver_wan: bool,
    auth: auth::Auth,
    stages: netstate::NetState,
    /// The download-order context (`ctx+7848`).
    dl: dlorder::DlOrder,
    /// The accounting side.
    pub billing: billing::Billing,
    /// The Wi-Fi units.
    pub wifi: wifi::Wifi,
}

impl fmt::Debug for Allnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Allnet").field("life", &self.life).field("phase", &self.auth.phase()).finish_non_exhaustive()
    }
}

/// What the main flow hands [`Allnet::update`] from the other managers each Operating frame.
#[derive(Debug, Clone, Copy)]
pub struct NeighbourState {
    /// The network manager is ready.
    pub network_ready: bool,
    /// The network manager's property setter (`0x140247080`) is running.
    pub setter_running: bool,
    /// The credit manager's state is 2.
    pub credit_ready: bool,
    /// The credit manager's error cell, the second argument of the original (`0x14023D930`).
    pub credit_error: ErrorCell,
}

impl Allnet {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter`: takes the three allnet configs, state 1.
    pub fn initialize(&mut self, cx: &mut Cx<'_>, game_mode: bool) {
        self.auth_config = cx.config.allnet_auth.clone();
        self.auth.initialize(cx, &self.auth_config, !game_mode);
        self.lan_modes = matches!(cx.config.network.location_router_directly, 0 | 2);
        self.location_mode = cx.config.network.location_router_directly;
        self.netinfo = NetInfo::default();
        self.stages = netstate::NetState::default();
        self.deliver_wan = false;
        self.dl = dlorder::DlOrder::new(&cx.config.netdeliver, cx.clock.tick_ms());
        self.app_status = 3;
        self.billing.initialize(cx);
        self.wifi.initialize(cx);
        self.arm();
    }

    /// `0x14023C510`, once per Operating frame: the auth phase machine (the Auth 2.0 leg talks to the keychip
    /// through `link`, the dongle manager's request layer), then billing with what the auth module shows it
    /// (and back), then the Wi-Fi units.
    ///
    /// `net` is the network manager's engine: one process-wide request slot, the same one the property setter
    /// uses.
    pub fn update(&mut self, cx: &mut Cx<'_>, link: &mut Link, net: &mut AmNetwork, neighbours: NeighbourState) {
        let NeighbourState { network_ready, setter_running, credit_ready, credit_error } = neighbours;
        if self.auth_config.enable && !self.pumping {
            // A credit error ended the set-up (`0x14023D930` stored it and set state 3 without a context): the
            // module stays done and nothing runs until a restart.
            if self.life.state == StateWord::FAILED {
                return;
            }
            // `0x14023D930`: with accounting on, the set-up waits for the credit manager; a credit error becomes
            // the module's own error and ends the set-up.
            if self.billing.mode() != 0 {
                if !credit_ready {
                    return;
                }
                if let Some((number, sub_number)) = credit_error.get() {
                    self.life.error.set(number, sub_number);
                    self.life.state = StateWord::FAILED;
                    return;
                }
            }
            // `0x14023DC70`: state 1 becomes 2; with the network manager's property setter running the check
            // counts as started and nothing else runs; otherwise the context is created once, then state 3.
            self.life.finish_setup();
            if setter_running {
                self.started = true;
                self.setup_done = true;
                return;
            }
            if !self.context {
                self.setup_done = true;
                self.context = true;
                self.started = true;
            }
            self.pumping = true;
        }
        if let Some(code) = self.auth.update(cx, link, network_ready, self.identity.as_ref()) {
            self.life.error.set(u16::try_from(code).unwrap_or(u16::MAX), 0);
        }
        self.stages.observe(&self.auth);
        let operating_mode = if self.billing.mode() == 3 { 2 } else { 1 };
        let link = self.auth.link(operating_mode);
        let back = self.billing.update(cx, &link);
        self.auth.apply(&back);
        self.wifi.update(cx);
        self.life.finish_setup();
        if self.auth_config.enable && self.life.state.is_done() {
            let title_router = self.auth.allnet_type() == 2;
            self.netinfo.step(net, cx.backend, title_router);
            // Slot 4 of the NetDeliver parameters (`allnetMgr[+1208] == 0`), written every frame the module runs.
            self.deliver_wan = self.auth.is_good();
            // `0x14028FAB0`: the download-order poll, after the authentication and the stage words.
            self.dl.update(cx.clock.tick_ms(), &mut self.auth);
        }
    }

    /// `0x14023C450`: releases the context, clears the auth info, state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        self.billing.terminate(cx);
        self.wifi.terminate(cx);
        self.auth.terminate();
        self.netinfo = NetInfo::default();
        self.stages = netstate::NetState::default();
        self.deliver_wan = false;
        self.dl.stop();
        self.context = false;
        self.started = false;
        self.setup_done = false;
        self.life.end();
    }

    /// `0x14023C620` (`allnet::Manager::reset`), the sweep's restart: with the alw context it runs the network-mode
    /// `alwRestart` `{8, testMode, 0, 0}` (`game_mode` is `0x14028F180`, the test flag is its negation), takes the
    /// accounting side back to its start, and sets app status 3; the error cell and the info block are cleared and
    /// the module goes back to state 1 (to 3 at once while `auth.enable` is off).
    pub fn restart(&mut self, game_mode: bool) {
        self.started = false;
        self.setup_done = false;
        if self.context {
            self.setup_done = true;
            let accounting = self.billing.mode() != 0;
            let restarted = self.auth.restart(auth::Restart::Network { test_mode: !game_mode }, accounting);
            if restarted.is_ok() {
                // `0x1403CD010`: the accounting library starts over with its context.
                if accounting {
                    self.billing.restart();
                }
                self.started = true;
            }
            self.set_app_status(3);
        }
        self.reset_info();
        self.arm();
    }

    /// `0x14023D0D0`, the network test's restart (network.md T.11.2): outside game mode, with the plain network
    /// manager's property setter running (`setter_running`, `0x140247080`), the check counts as started; with the
    /// alw context, `alwRestart` `{1, 1, 0, 0}` re-checks the network. Both clear the error cell and the info
    /// block. Only a ready manager with `auth.enable` acts.
    pub fn restart_test(&mut self, game_mode: bool, setter_running: bool) {
        self.started = false;
        if !self.auth_config.enable || !self.life.state.is_done() {
            return;
        }
        if !game_mode && setter_running {
            self.started = true;
            self.life.error.clear();
            self.reset_info();
        }
        if self.context && self.auth.restart(auth::Restart::Recheck, self.billing.mode() != 0).is_ok() {
            self.life.error.clear();
            self.reset_info();
            self.started = true;
        }
    }

    /// `state := 1` when `auth.enable`, else `3` (`0x14023C230`, `0x14023C620`).
    fn arm(&mut self) {
        self.life.begin();
        self.pumping = !self.auth_config.enable;
        if self.pumping {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x14023D860`: the info block back to its empty state: property, detection words and the auth info, the
    /// accounting slot table, the Wi-Fi unit list; the WAN flag is not available.
    fn reset_info(&mut self) {
        self.netinfo = NetInfo::default();
        self.stages = netstate::NetState::default();
        self.auth.clear_info();
        self.billing.clear_slots();
        self.wifi.clear_units();
        self.deliver_wan = false;
    }

    /// `alwSetAppStatus` (`0x1403B42F0`): 3 game running, 4 not.
    fn set_app_status(&mut self, status: u32) {
        self.app_status = status;
        self.billing.set_app_status(status);
    }

    /// The network test's view of the stage words (`ALLNET+1512`, network.md T.11.2): the IP, gateway and DNS-LAN
    /// stages, the hop count (-1 pending) and the line-type stage, as `[f0, f1, f2, f4, f5]`.
    pub fn test_stages(&self) -> [i32; 5] {
        let stages = &self.stages;
        [stages.ip, stages.gateway, stages.dns, stages.hops, stages.line]
    }

    /// The network test's `f7 == 1 && f8 == 4`: the authentication reached the status "authenticated".
    pub fn test_authenticated(&self) -> bool {
        self.stages.authenticated
    }

    /// The network test's current error record (`ALLNET+1212`, `0x14023DE70`): the latched alw error number.
    pub fn test_error(&self) -> ErrorCell {
        let mut record = ErrorCell::EMPTY;
        if self.auth_config.enable && self.auth.latched() != 0 {
            record.set(u16::try_from(self.auth.latched()).unwrap_or(u16::MAX), 0);
        }
        record
    }

    /// `ALLNET+1204` (`0x14023D820`): the restart was accepted, either because the alw context was created or
    /// restarted, or because the network manager's check sequence runs. Read only while the manager is ready.
    pub fn test_started(&self) -> bool {
        self.life.state.is_done() && self.started
    }

    /// `ALLNET+1205` (`0x14023D840`): the first pass of the set-up ran (the context was requested). Read only while
    /// the manager is ready.
    pub fn test_setup_done(&self) -> bool {
        self.life.state.is_done() && self.setup_done
    }

    /// `app status` (`0x14023D7E0`): with `auth.enable`, alw app status `4 - (idle != 0)`.
    pub fn refresh_idle(&mut self, idle: bool) {
        self.idle = idle;
        if self.auth_config.enable {
            self.app_status = 4 - u32::from(idle);
            self.billing.set_app_status(self.app_status);
        }
    }

    /// The alw app status (`3` game running, `4` not).
    pub fn app_status(&self) -> u32 {
        self.app_status
    }

    /// The keychip and game identity the platform managers publish (keychip and board serial, game id and
    /// version, function type). The authentication waits for it in its set-up phase.
    pub fn set_identity(&mut self, identity: KeychipIdentity) {
        self.identity = Some(identity);
    }

    /// The game version text of the init record (`allnet.md` part 1, record `+21`): `"%u.%02u"` of the selected
    /// application image (`image` = `(major, minor)`), `0.00` without one; the configured `develop_version`
    /// replaces it when the platform develop flag is set and the configured version is present. With the
    /// multi-auth flag (`AuthConfig+8`, `develop_version` in the config struct) and a non-zero major, the major
    /// parity is forced: incremented when accounting is on and the major is odd, or accounting is off and it is
    /// even. The record field is `char[6]`, so the text is cut to five characters.
    pub fn game_version(&self, image: Option<(u8, u8)>, develop: bool) -> String {
        let configured = &self.auth_config.version;
        let (mut major, minor) = match image {
            _ if develop && configured.valid => (configured.packed >> 16, (configured.packed >> 8) & 0xFF),
            Some((major, minor)) => (u32::from(major), u32::from(minor)),
            None => (0, 0),
        };
        if self.auth_config.develop_version && major != 0 && (self.billing.mode() != 0) == (major % 2 == 1) {
            major += 1;
        }
        let mut text = format!("{major}.{minor:02}");
        text.truncate(5);
        text
    }

    /// The function type byte (`kk`, `functype=`): the accounting type of the init record (`+0 u8`, `0x14028FEC0`):
    /// mode 1 gives 1, modes 2 and 3 give 2, otherwise 0. It is the byte the accounting library receives as
    /// `alibExInitialize` parameter `+144` and stores at `ctx+865612`.
    pub fn function_type(&self) -> u8 {
        match self.billing.mode() {
            1 => 1,
            2 | 3 => 2,
            _ => 0,
        }
    }

    /// `changeApp` game-id hand-over: with accounting enabled the game id becomes the APM sub game id.
    pub fn change_app(&mut self, game_id: GameId) -> AmResult {
        if self.billing.mode() != 0 {
            self.apm_game_id = Some(game_id.to_string());
        }
        Ok(())
    }

    /// `0x14023CE60`: ALL.Net state 2..3, no pending error and, with accounting, no queued accounting request.
    pub fn apm_change_ready(&self) -> bool {
        self.life.state.is_done()
            && self.life.error.is_empty()
            && (self.billing.mode() == 0 || self.billing.queue_is_empty())
    }

    /// `changeApp` (`0x14023CF20`) with the version of storage.md 5.4 step 10 (`raw` as `AppVersion::raw`): fails
    /// unless [`Self::apm_change_ready`]; with accounting it sets the APM sub game id (4 characters) and the
    /// game version (the short text `major.minor`, `--.--` for raw 0).
    pub fn apm_change(&mut self, game_id: GameId, version_raw: u32) -> AmResult {
        if !self.apm_change_ready() {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.billing.mode() != 0 {
            let (major, minor, _) = crate::apm::version_parts(version_raw);
            self.apm_game_id = Some(game_id.to_string());
            self.apm_version = Some(if version_raw == 0 { "--.--".to_owned() } else { format!("{major}.{minor:02}") });
        }
        Ok(())
    }

    /// The APM sub game id set by [`Self::change_app`] / [`Self::apm_change`].
    pub fn apm_game_id(&self) -> Option<&str> {
        self.apm_game_id.as_deref()
    }

    /// The APM game version text set by [`Self::apm_change`].
    pub fn apm_version(&self) -> Option<&str> {
        self.apm_version.as_deref()
    }

    /// `cancel-credit` (`0x14023CD00`): `alwCancelPlay`; a failure is only logged by the original.
    pub fn cancel_credit(&mut self) {
        let _ = self.billing.cancel_play();
    }

    /// The auth result is good (network.md L.14).
    pub fn is_good(&self) -> bool {
        self.auth.is_good()
    }

    /// `AuthInfo +0x0C` (`0x14028E460`): the client id, 0 until authenticated.
    pub fn client_id(&self) -> u32 {
        self.auth.client_id()
    }

    /// The authentication state the ports publish.
    pub fn auth(&self) -> &auth::Auth {
        &self.auth
    }

    /// `allReady` term (`0x14023D1E0`).
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`0x14023D200`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// Whether the last idle refresh said idle.
    pub fn idle(&self) -> bool {
        self.idle
    }

    /// `auth.enable`.
    pub fn auth_enabled(&self) -> bool {
        self.auth_config.enable
    }

    /// Whether the module gate and state allow the auth facade to answer (`state` in `{2, 3}`).
    pub fn running(&self) -> bool {
        self.life.state.is_done() && self.auth_config.enable
    }

    /// `isLanAvailable` of the ALL.Net manager (`0x14023D240`, network.md M.11): the module is done, the adapter
    /// address is in the keychip network (`N+236 == 2`) and `location_router_directly` is deny (0) or allow (2).
    pub fn is_lan_available(&self) -> bool {
        self.life.state.is_done() && self.lan_modes && self.netinfo.class() == Some(NetClass::Keychip)
    }

    /// The property slot of the manager (`0x14023D220`): the adapter properties were read (`+1172` flag).
    pub fn has_property(&self) -> bool {
        self.netinfo.property().is_some()
    }

    /// The property the network facades report while ALL.Net is the active manager (`0x14023D220`: `+1176` once
    /// the `+1172` flag is set).
    pub fn network_property(&self) -> Option<NetworkProperty> {
        self.netinfo.property().map(|buffer| NetworkProperty {
            dhcp: buffer.flags & PropertyBuffer::DHCP != 0,
            address: buffer.ip,
            subnet_mask: buffer.mask,
            gateway: buffer.gateway,
            primary_dns: buffer.dns1,
            secondary_dns: buffer.dns2,
        })
    }

    /// `isLocationLanAvailable` of the ALL.Net manager (`0x14023D2B0`, network.md M.11): the module is done, the
    /// hop count of the stage words is known (`0x14028E470(+1512) >= 0`) and either the adapter address is in the
    /// keychip network or `location_router_directly` is not deny.
    pub fn is_location_lan_available(&self) -> bool {
        self.life.state.is_done()
            && self.stages.hops >= 0
            && (self.netinfo.class() == Some(NetClass::Keychip) || self.location_mode != 0)
    }

    /// `isWanAvailable` of the ALL.Net manager (`0x14023D330`): the WAN word `+1208` is 0, i.e. the authentication
    /// is good; no state check.
    pub fn is_wan_available(&self) -> bool {
        self.deliver_wan
    }

    /// `amRtcIsServerSync`: the RTC holds a server time.
    pub fn server_time_synced(&self, clock: &dyn Clock) -> bool {
        self.auth.server_time_synced(clock)
    }

    /// `am::allnet::NetDeliverParam` (network.md D.5): the eight slots NetDeliver reads in non-local mode.
    pub fn deliver_auth_param(&self) -> DeliverAuthParam {
        let auth = &self.auth;
        let text = auth.text().map_or_else(String::new, |text| {
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            String::from_utf8_lossy(&text[..end]).into_owned()
        });
        // Slot 8: the download-order key, once the context runs.
        let uri = self.dl.key().map(str::to_owned);
        DeliverAuthParam {
            // Slot 1: `auth+36`, the "generation became 3" flag.
            auth_restarted: auth.restarted(),
            // Slot 2: `auth.phase(+32)` in {5, 6} or the download-order context state in {2, 3}.
            ready: matches!(auth.generation(), 5 | 6) || self.dl.finished(),
            // Slot 3: `ctx+536`, the key changed in the last poll.
            uri_restarted: self.dl.key_changed(),
            // Slots 4 and 5: the time is only meaningful while the last authentication was good.
            auth_time: auth.time().filter(|slot| slot.valid && self.deliver_wan).map(|slot| slot.tm),
            // Slot 6: `auth+24`, the line mask.
            line_code: auth.line_mask(),
            // Slot 7: `auth+37`, the Auth 2.0 text.
            auth_data: text,
            uri,
        }
    }
}
