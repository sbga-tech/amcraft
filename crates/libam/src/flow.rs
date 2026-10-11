//! The main flow (`am::MainFlow`, `spec/libam/core.md` M.1-M.5): `initialize`, one frame per `execute`,
//! `finalize`, and the two states Checking and Operating.
//!
//! * **Checking** (`CheckingCabinetTypeState`): runs the keychip, system I/O, platform and storage set-up every
//!   frame. When platform, keychip and storage are ready it either latches the first module error and moves on
//!   with the default configuration, or fires `LoadCabinetType` and waits for the daemon's answer
//!   ([`FlowState::AwaitingAnswer`]). The answer ([`Core::answer_cabinet_type`]) applies the fix-ups of
//!   `core.md` 6 ([`apply_fixups`]), validates, adopts the configuration or latches 4104, and enters Operating
//!   inside the same call.
//! * **Operating**: [`Core::enter_operating`] hands the configuration to the modules in the order of the table
//!   of `core.md` 7; [`Core::operating_update`] runs the modules in the order of M.3 and then the `allReady`
//!   transition from the set-up process state to advertise (1 -> 3) or test (2 -> 5).

use libam_api::am_config::{AmConfig, UNSET_U64};
use libam_api::interface::{CabinetAppConfig, LibamEvent, LoadCabinetTypeEvent};
use libam_api::result::{AmError, AmResult};

use crate::abaas::LogInputs;
use crate::allnet::services::KeychipIdentity;
use crate::core::{Core, cx};
use crate::credit::{CoinAdapter, CoinSlots, CreditMode, NoCoinSlots};
use crate::emoney::EMoneyInputs;
use crate::network::laninstall::NetInputs;
use crate::network::netdeliver::{DeliverAuthParam, DeliverInputs};
use crate::network::networktest::PeerView;
use crate::nvram::SetupInputs;
use crate::platform::dongle::DonglePermissions;

/// The error latched for a refused or invalid configuration (M.6).
const ERROR_BAD_CONFIG: u16 = 4104;
/// Process state "setup Game Mode".
const STATE_GAME_SETUP: u32 = 1;

/// Where the main flow is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowState {
    /// Not started: before `initialize`, after `finalize`, or after the platform bail-out (error 901).
    #[default]
    NotStarted,
    /// `CheckingCabinetTypeState`, the cabinet-type request not sent yet.
    Checking,
    /// `CheckingCabinetTypeState`, `LoadCabinetType` sent; waiting for `load_cabinet_type_result`.
    AwaitingAnswer,
    /// `OperatingState`.
    Operating,
}

impl FlowState {
    /// The aggregator's `inChecking` (M.6).
    pub fn in_checking(self) -> bool {
        matches!(self, Self::Checking | Self::AwaitingAnswer)
    }
}

/// The fix-ups libam applies to the converted configuration before the validator (`core.md` 6, F1-F5), driven by
/// the keychip's permission bytes.
pub fn apply_fixups(config: &mut AmConfig, permissions: DonglePermissions) {
    // F1: no ALL.Net permission switches the ALL.Net based services off.
    if config.allnet_auth.enable && !permissions.allnet {
        config.allnet_auth.enable = false;
        config.allnet_wifi.enable = false;
        if config.netdeliver.enable && !config.netdeliver.local {
            config.netdeliver.enable = false;
        }
        config.aimepay.enable = false;
    }
    // F2: no accounting permission.
    if !permissions.accounting {
        config.allnet_accounting.mode = 0;
    }
    if config.allnet_auth.enable {
        // F3: ALL.Net needs the network module.
        if !config.network.enable {
            config.network.enable = true;
            config.network.use_gateway = true;
        }
        // F4.
        config.network.use_location_router = !permissions.title_router;
    }
    // F5: accounting needs the credit module, with every coin box unassigned.
    if !config.credit.enable && config.allnet_accounting.mode != 0 {
        config.credit.enable = true;
        config.credit.coin_box_assign = [UNSET_U64; 8];
    }
}

impl Core {
    /// `Interface::initialize` (M.1): code 2 when already initialised; otherwise constructs the modules, runs
    /// the debug and platform initialisation and, unless the platform bailed out (901), starts the main flow
    /// (`CheckingCabinetTypeState::enter`). Returns the events fired meanwhile.
    pub fn initialize(&mut self) -> AmResult<Vec<LibamEvent>> {
        if self.exists {
            return Err(AmError::code(AmError::NOT_INITIALIZED));
        }
        self.construct_modules();
        self.platform.initialize(&mut self.backend, &mut self.nvram);
        if !self.platform.bailed_out() {
            self.enter_checking();
        }
        self.exists = true;
        Ok(self.events.take())
    }

    /// `Interface::execute` (M.1): one main-flow step, then the error aggregator. The events of the frame.
    pub fn execute(&mut self) -> Vec<LibamEvent> {
        if !self.exists {
            return Vec::new();
        }
        match self.flow {
            FlowState::NotStarted | FlowState::AwaitingAnswer => {
                if self.flow == FlowState::AwaitingAnswer {
                    self.update_checking_modules();
                }
            }
            FlowState::Checking => self.checking_update(),
            FlowState::Operating => self.operating_update(),
        }
        self.aggregate_errors(self.flow.in_checking());
        self.events.take()
    }

    /// `Interface::finalize` (M.1): the terminate order of M.3; idempotent.
    pub fn finalize(&mut self) {
        if !self.exists {
            return;
        }
        self.terminate_modules();
        self.exists = false;
        self.flow = FlowState::NotStarted;
    }

    /// `CheckingCabinetTypeState::enter` (`0x14029C830`): error manager, keychip, system I/O, sequence (which
    /// fires `ChangeState(1)`) and storage.
    fn enter_checking(&mut self) {
        let sub_storage = self.platform.sub_storage();
        self.error.enter(&mut self.master);
        self.dongle.enter();
        self.systemio.enter(&mut cx!(self), sub_storage);
        self.sequence.enter(&mut self.events);
        self.storage.enter();
        self.flow = FlowState::Checking;
    }

    /// The four set-up steps `CheckingCabinetTypeState::update` runs every frame (M.3).
    fn update_checking_modules(&mut self) {
        self.dongle.update(&mut cx!(self));
        self.systemio.update(&mut cx!(self));
        self.platform.update(&mut cx!(self));
        let (ready, develop, game_id) = (self.dongle.is_done(), self.dongle.develop(), self.dongle.game_id());
        self.storage.update(&mut cx!(self), ready, false, develop, game_id);
    }

    /// `CheckingCabinetTypeState::update` (`0x14029C9B0`).
    fn checking_update(&mut self) {
        self.update_checking_modules();
        if !(self.platform.is_ready() && self.dongle.is_done() && self.storage.is_ready()) {
            return;
        }
        let cells = [
            self.dongle.error_cell(),
            self.systemio.error_cell(),
            self.platform.error_cell(),
            self.storage.error_cell(),
        ];
        if let Some((number, sub_number)) = cells.into_iter().find_map(|cell| cell.get()) {
            // A module error: latched on the error manager, no configuration is requested.
            self.latch_error(number, sub_number);
            self.enter_operating();
            return;
        }
        let event = LoadCabinetTypeEvent {
            model: self.dongle.model(),
            dip_switch: self.systemio.dip() as u16,
            region: self.dongle.region(),
            app_config: CabinetAppConfig::default(),
        };
        self.events.push(LibamEvent::LoadCabinetType(Box::new(event)));
        self.flow = FlowState::AwaitingAnswer;
    }

    /// The daemon's answer to `LoadCabinetType` (M.5): refused, invalid or accepted. The Checking -> Operating
    /// transition and [`Self::enter_operating`] happen inside this call. Ignored unless the request is pending.
    pub fn answer_cabinet_type(&mut self, accepted: bool, app_config: CabinetAppConfig) {
        if !self.exists || self.flow != FlowState::AwaitingAnswer {
            return;
        }
        let mut adopted = None;
        if accepted {
            let mut config = app_config.effective();
            apply_fixups(&mut config, self.dongle.permissions());
            if config.validate().is_ok() {
                adopted = Some(config);
            }
        }
        match adopted {
            Some(config) => self.config = config,
            None => self.latch_error(ERROR_BAD_CONFIG, 0),
        }
        self.enter_operating();
    }

    /// `0x140233090(errmgr, number, sub)`: the error manager's own raise, without the sequence reset.
    fn latch_error(&mut self, number: u16, sub_number: u16) {
        let time = self.now_tm();
        self.error.raise(&mut cx!(self), &mut self.master, number, sub_number, time);
    }

    /// `OperatingState::enter` (`0x14029CF30`): the configuration reaches the modules in the order of the table
    /// of `core.md` 7.
    fn enter_operating(&mut self) {
        self.flow = FlowState::Operating;
        // 1: ResourceSet::initialize fails (and latches 4104) when the resource path is empty (7.1).
        if self.config.common.resource.resource_path.as_str().is_empty() {
            self.latch_error(ERROR_BAD_CONFIG, 0);
        }
        // 2: the debug manager hook belongs to a module that is not part of the flow.
        // 3: the record initialiser opens the application file of the configured game id (`0x14029C060`); a
        // failure is the mapped context error 4105.
        if self.nvram.init_records(&mut self.backend, self.dongle.game_id()).is_err() {
            self.latch_error(4105, 0);
        }
        let game_mode = self.sequence.is_game_mode();
        let mut cx = cx!(self);
        self.error.configure(&cx); // 4
        // `ctx[26180]`, the platform's sub-storage byte: the record flag of AppInfo, Aime and e-money (84 or 87).
        let record_flag = self.platform.sub_storage();
        self.jvs.initialize(&mut cx); // 5
        self.can.initialize(&mut cx); // 6
        self.usbio.initialize(&mut cx); // 7
        self.appimage.initialize(&mut cx); // 8
        self.hwm.initialize(&mut cx); // 9
        self.app_info.initialize(cx.config.nvram, record_flag); // 10
        self.credit.initialize(&mut cx, record_flag); // 11
        self.sequence.configure(&cx); // 12
        self.usbdevice.initialize(&mut cx); // 13
        self.video.initialize(&mut cx); // 14
        self.storage.configure(&cx); // 15
        self.display.initialize(&mut cx); // 16
        self.location_router = cx.config.allnet_auth.enable; // 17
        self.network.initialize(&mut cx, record_flag); // 18
        self.allnet.initialize(&mut cx, game_mode); // 19
        self.laninstall.initialize(&mut cx); // 20
        self.aime.initialize(&mut cx, record_flag); // 21
        if let (Some(game_id), Some(serial)) = (self.dongle.game_id(), self.dongle.keychip_short_serial()) {
            self.aime.set_board(game_id, serial);
        }
        self.emoney.initialize(&mut cx, record_flag); // 22
        self.aimepay.initialize(&mut cx); // 23
        self.abaas.initialize(&mut cx); // 24
        self.apm.initialize(&mut cx); // 25
        self.networktest.initialize(&mut cx); // 26
    }

    /// `OperatingState::update` (`0x14029D590`, M.3): strictly this order, then the `allReady` transition.
    fn operating_update(&mut self) {
        self.platform.update(&mut cx!(self));
        self.systemio.update(&mut cx!(self));
        self.jvs.update(&mut cx!(self));
        self.can.update();
        self.usbio.update(&mut cx!(self));
        self.dongle.update(&mut cx!(self));
        self.hwm.update(&mut cx!(self));
        let dongle_done = self.dongle.is_done();
        let platform_done = self.platform.is_ready();
        let (develop, game_id, region) = (self.dongle.develop(), self.dongle.game_id(), self.dongle.region());
        // The record flag the sequence and error managers' record mapping reads (`ctx[26180]`), not the keychip's.
        let record_flag = self.platform.sub_storage();
        self.sequence.update(&mut cx!(self), platform_done && dongle_done, region, game_id, record_flag);
        let sequence_ready = self.sequence.is_ready();
        self.storage.update(
            &mut cx!(self),
            dongle_done && sequence_ready,
            self.sequence.first_launch(),
            develop,
            game_id,
        );
        self.error.update(&mut cx!(self), &mut self.master, dongle_done, record_flag, game_id);
        self.video.update(&mut cx!(self));
        let error_ready = self.error.is_ready();
        self.usbdevice.update(&mut cx!(self), error_ready);
        let app_info_ready = self.sequence.is_ready() && dongle_done && platform_done;
        let inputs = SetupInputs { ready: app_info_ready, first_launch: self.sequence.first_launch(), region, game_id };
        self.app_info.update(&cx!(self), inputs);
        let (keychip_done, keychip_error) = (self.dongle.is_done(), self.dongle.error_cell());
        self.network.update(&mut cx!(self), keychip_done, keychip_error);
        self.update_credit();
        let auth_enabled = self.config.allnet_auth.enable;
        let network_ready = self.network.is_ready();
        // The identity the alw init record carries (allnet.md part 1): published once the keychip is done.
        if dongle_done && let (Some(keychip), Some(game_id)) = (self.dongle.keychip_short_serial(), game_id) {
            let identity = KeychipIdentity {
                keychip: keychip.to_owned(),
                // Empty until the computer name is a valid serial: the original copies the platform's text as is.
                board: self.platform.board_short_serial().unwrap_or_default(),
                game_id: game_id.to_string(),
                game_version: self.allnet.game_version(LogInputs::version_of(self.appimage.current_app()), develop),
                function_type: self.allnet.function_type(),
            };
            self.allnet.set_identity(identity);
        }
        let neighbours = crate::allnet::NeighbourState {
            network_ready,
            setter_running: self.network.property_setter_running(),
            credit_ready: self.credit.is_ready(),
            credit_error: self.credit.error_cell(),
        };
        self.allnet.update(&mut cx!(self), self.dongle.link(), self.network.engine_mut(), neighbours);
        self.appimage.update(&mut cx!(self));
        let active_net_ready = if auth_enabled { self.allnet.is_ready() } else { network_ready };
        let net_inputs = self.net_inputs(auth_enabled);
        self.laninstall.update(&mut cx!(self), active_net_ready, &net_inputs);
        let auth_good = self.allnet.is_good();
        let location_id = self.allnet.auth().location().map_or(0, |location| location.id);
        self.aime.update(&mut cx!(self), auth_good, location_id);
        self.update_emoney(auth_enabled);
        self.update_abaas();
        crate::aimepay::update(self);
        self.update_apm();
        // `isGameMode`: the process state is 1, 3 or 4 (setup game mode, advertise, play); `modulesReady`: the
        // network manager is ready and the ALL.Net state is 2 or 3 (network.md T.4.4), not the `allReady` term.
        let game_mode = matches!(self.sequence.process_state(), 1 | 3 | 4);
        let modules_ready = self.network.is_ready() && self.allnet.is_ready();
        let peers = PeerView::of(self);
        self.networktest.update(&mut cx!(self), game_mode, modules_ready, &self.network, &peers);
        self.display.update(&mut cx!(self));
        self.advance_process_state();
    }

    /// What LAN install and NetDeliver read from the other modules (`0x14029D590`, network.md L.3.3 and D.5):
    /// the facades and the NetDeliver parameter come from the active network manager, `allnet::Manager` when
    /// `auth.enable`, else `am::network::Manager`.
    fn net_inputs(&self, auth_enabled: bool) -> NetInputs {
        let (lan_available, network_property) = if auth_enabled {
            (self.allnet.is_lan_available(), self.allnet.has_property())
        } else {
            (self.network.is_lan_available(), self.network.property().is_some())
        };
        let clock = &*self.clock;
        NetInputs {
            deliver: DeliverInputs {
                serial: self.dongle.keychip_short_serial().unwrap_or_default().to_owned(),
                game_id: self.dongle.game_id().map(|id| id.to_string()).unwrap_or_default(),
                platform_id: self.platform.id().map(|id| id.name.clone()).unwrap_or_default(),
                server_time_synced: self.allnet.server_time_synced(clock),
                lan_available,
            },
            auth: if auth_enabled { self.allnet.deliver_auth_param() } else { DeliverAuthParam::default() },
            network_property,
        }
    }

    /// The credit update with the node adapter chosen JVS > USB I/O > CAN by its enabled flag (`credit.md`).
    fn update_credit(&mut self) {
        let no_error = self.error.current().number == 0;
        let mode = CreditMode { play: self.sequence.is_game() && no_error, test: self.sequence.is_test() };
        let mut none = NoCoinSlots;
        let adapter: &mut dyn CoinSlots =
            match CoinAdapter::select(self.jvs.enabled(), self.usbio.enabled(), self.can.enabled()) {
                CoinAdapter::Jvs => &mut self.jvs,
                CoinAdapter::UsbIo => &mut self.usbio,
                CoinAdapter::Can => &mut self.can,
                CoinAdapter::None => &mut none,
            };
        self.credit.update(&mut cx!(self), adapter, mode);
    }

    /// The e-money update with what `0x14029D590` hands it: `a2` (dongle, credit == 2, storage, platform, Aime and
    /// display ready), the 11-character board serial (`a3`), `a6` (slot 4 `isLanAvailable` OR slot 5
    /// `isLocationLanAvailable` of the active network manager), the dongle's 11-character keychip serial (`a7`,
    /// `ctx+26218`), the credit counters and coin values and the
    /// display state; the credit module takes the coins of a paid coin deal. The reader lease (the token byte
    /// `+16`) is the store-card reader's: the module hands it the Aime unit `emoney.aime_unit` names once the aime
    /// module has opened its ports, and it is unavailable while an Aime sequence holds the unit.
    fn update_emoney(&mut self, auth_enabled: bool) {
        /// The value of one coin of a JVS slot (`credit+1248`, `credit.md` 2.3).
        const SLOT_COIN_VALUE: u32 = 100;
        let modules_ready = self.dongle.is_done()
            && self.credit.is_ready()
            && self.storage.is_ready()
            && self.platform.is_ready()
            && self.aime.is_ready()
            && self.display.is_ready();
        let online = if auth_enabled {
            self.allnet.is_lan_available() || self.allnet.is_location_lan_available()
        } else {
            self.network.is_lan_available() || self.network.is_location_lan_available()
        };
        let mut inputs = EMoneyInputs {
            modules_ready,
            online,
            serial: self.platform.board_short_serial(),
            keychip: self.dongle.keychip_short_serial().map(str::to_owned),
            model: self.platform.id().and_then(|id| {
                let fourth = if id.number < 10 { char::from_digit(id.number, 10) } else { char::from_u32(id.number) };
                fourth.map(|fourth| format!("{}{fourth}", id.name))
            }),
            display_ready: self.display.ready(),
            display_busy: self.display.is_mode_a(),
            ..EMoneyInputs::default()
        };
        if self.emoney.is_enabled() {
            if self.emoney.store_reader_wants_unit()
                && let Some(unit) = self.aime.store_reader(self.emoney.aime_unit())
            {
                self.emoney.attach_store_reader(unit);
            }
            inputs.reader_available = self.emoney.store_reader_free();
        }
        for player in 0..8u32 {
            let slot = player as usize;
            inputs.coin_counters[slot] = self.credit.bookkeeping_coin(player).unwrap_or(0);
            if self.credit.player_value(player).is_some() {
                inputs.coin_prices[slot] = SLOT_COIN_VALUE;
            }
        }
        self.emoney.update_with(&mut cx!(self), &inputs, &mut self.credit);
        // `AimePayInfo::setSmsEnabled` (`0x1402611F0`): the e-money module's sales-reporting flag, which the Aime
        // Pay module reads for its deal-blocked predicate (`0x14023B7F0`) later in this frame.
        self.aimepay.set_sms_enabled(self.emoney.sms_enabled());
        for op in self.emoney.take_display_ops() {
            op.apply(&mut self.display);
        }
    }

    /// The `allReady` transition of M.2: while the process state is 1 or 2 and every module is ready, the
    /// sequence moves to advertise (1 -> 3) or test (2 -> 5), the terminals are suspended when an error is
    /// current, the first-launch step runs and a pending display reboot is requested.
    fn advance_process_state(&mut self) {
        let state = self.sequence.process_state();
        if !matches!(state, 1 | 2) || !self.all_ready() {
            return;
        }
        let new = if state == STATE_GAME_SETUP { 3 } else { 5 };
        self.sequence.set_state(new, &mut self.events);
        if self.error.current().number != 0 {
            // An error is current: the enabled terminals are suspended (`0x14022A530`, `0x14022FE00`,
            // `0x1402398F0`; the AimePay call is gated on the Aime byte like the first).
            if self.aime.enabled() {
                self.aime.set_permit(false, true);
            }
            if self.emoney.is_enabled() {
                self.emoney.set_permit(false, true);
            }
            if self.aime.enabled() {
                self.aimepay.set_permit(false, true);
            }
        }
        if self.sequence.first_launch() {
            self.error.clear_logs(&self.nvram);
            self.aime.clear_play_log();
            let (region, game_id) = (self.dongle.region(), self.dongle.game_id());
            if self.app_info.error_cell().is_empty() && self.storage.error_cell().is_empty() {
                self.sequence.store_history(&self.nvram, region, game_id);
            }
        }
        if self.video.wants_reboot() {
            self.platform.reboot(&mut cx!(self));
        }
    }

    /// `MainFlow::terminate` (`0x14029E350`, M.3): the modules in reverse construction order, the launcher
    /// hand-off inputs just before the error manager's release, the platform last.
    fn terminate_modules(&mut self) {
        let test_mode = self.sequence.is_test();
        let mut cx = cx!(self);
        self.networktest.terminate(&mut cx);
        self.apm.terminate(&mut cx);
        self.abaas.terminate(&mut cx);
        self.aimepay.terminate(&mut cx);
        self.emoney.terminate(&mut cx);
        self.aime.terminate(&mut cx);
        let download_ui = self.laninstall.download_ui();
        self.laninstall.terminate(&mut cx);
        self.allnet.terminate(&mut cx);
        self.network.terminate(&mut cx);
        self.display.terminate(&mut cx);
        self.storage.terminate();
        self.video.terminate(&mut cx);
        self.usbdevice.terminate(&mut cx);
        self.sequence.terminate(&mut cx);
        self.credit.terminate(&mut cx);
        self.app_info.terminate();
        self.hwm.terminate(&mut cx);
        self.appimage.terminate(&mut cx);
        self.usbio.terminate(&mut cx);
        self.can.terminate(&mut cx);
        self.jvs.terminate(&mut cx);
        self.systemio.terminate();
        self.dongle.terminate(&mut cx);
        // [I] the sequence test flag and the LAN-install result are the master inputs (core.md A.6).
        self.master.set_test_flag(test_mode);
        self.master.set_download_ui(download_ui);
        self.error.terminate(&mut cx, &mut self.master);
        self.platform.terminate(&mut self.nvram);
    }
}
