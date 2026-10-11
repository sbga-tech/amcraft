//! The shared libam state: every module instance, the adopted configuration, the event queue and the platform
//! gate state (`spec/libam/core.md` M.4, M.6, C).
//!
//! [`Core`] is what every port handle reaches through [`crate::shared::Shared`]. The main flow ([`crate::flow`])
//! drives it; this file holds the data and the cross-module operations that are not part of the flow itself:
//! the error raise path (`0x140253370`), the idle/busy refresh (`0x140254C40`) and the error aggregator
//! (`0x140253D30`, M.6).

use crate::config::AmConfig;
use crate::time::Tm;

use crate::abaas::Abaas;
use crate::aime::Aime;
use crate::aimepay::AimePay;
use crate::allnet::Allnet;
use crate::apm::Apm;
use crate::appimage::AppImage;
use crate::can::Can;
use crate::clock::Clock;
use crate::credit::Credit;
use crate::display::Display;
use crate::emoney::EMoney;
use crate::error::ErrorManager;
use crate::flow::FlowState;
use crate::jvs::Jvs;
use crate::master::Master;
use crate::module::{ErrorCell, EventQueue};
use crate::network::{NetInstall, Network, NetworkTest};
use crate::nvram::{AppInfoManager, NvramContext};
use crate::platform::Platform;
use crate::platform::dongle::Dongle;
use crate::platform::hwm::Hwm;
use crate::platform::storage::Storage;
use crate::platform::video::Video;
use crate::sequence::Sequence;
use crate::usbio::{SystemIo, UsbDevice, UsbIo};
use crate::util::tm_from_date_time;
use backend::Backend;

/// Builds the hook context from a [`Core`]'s backend, internal clock and shared state. A macro rather than a
/// method so the borrow covers exactly the context fields and a module field can be borrowed next to them:
/// `self.jvs.update(&mut cx!(self))`.
macro_rules! cx {
    ($core:ident) => {
        $crate::module::Cx {
            backend: &mut $core.backend,
            clock: &*$core.clock,
            events: &mut $core.events,
            config: &$core.config,
            nvram: &$core.nvram,
        }
    };
}
pub(crate) use cx;

/// Everything libam owns while it is initialised, and the state of the main flow.
pub struct Core {
    /// `g[0]`: libam is initialised (set by `initialize`, cleared by `finalize`).
    pub exists: bool,
    /// Everything outside the process.
    pub backend: Backend,
    /// Libam's internal source of time.
    pub clock: Box<dyn Clock>,
    /// Events produced since the last `initialize` / `execute` returned.
    pub events: EventQueue,
    /// The configuration the main flow adopted; the default until the cabinet-type answer is accepted.
    pub config: AmConfig,
    /// The main flow state.
    pub flow: FlowState,
    /// `ctx+495360`: the location-router byte, written from `auth.enable` at Operating entry.
    pub location_router: bool,
    /// The launcher hand-off (`core.md` A, B).
    pub master: Master,
    /// The NVRAM system-file context shared by the platform record and the NVRAM-backed modules.
    pub nvram: NvramContext,
    /// `ctx+13888`.
    pub error: ErrorManager,
    /// `ctx+25680`.
    pub platform: Platform,
    /// `ctx+26184`.
    pub dongle: Dongle,
    /// `ctx+26640`.
    pub jvs: Jvs,
    /// `ctx+32536`.
    pub can: Can,
    /// `ctx+35528`.
    pub usbio: UsbIo,
    /// `ctx+55192`.
    pub appimage: AppImage,
    /// `ctx+126216`.
    pub hwm: Hwm,
    /// `ctx+126544`.
    pub app_info: AppInfoManager,
    /// `ctx+129336`.
    pub credit: Credit,
    /// `ctx+133552`.
    pub sequence: Sequence,
    /// `ctx+135640`.
    pub systemio: SystemIo,
    /// `ctx+136576`.
    pub usbdevice: UsbDevice,
    /// `ctx+138216`.
    pub video: Video,
    /// `ctx+138608`.
    pub storage: Storage,
    /// `ctx+140656`.
    pub display: Display,
    /// `ctx+145840`.
    pub network: Network,
    /// `ctx+150952`.
    pub allnet: Allnet,
    /// `ctx+159376`.
    pub laninstall: NetInstall,
    /// `ctx+172272`.
    pub aime: Aime,
    /// `ctx+218624`.
    pub emoney: EMoney,
    /// `ctx+309592`.
    pub aimepay: AimePay,
    /// `ctx+488640`.
    pub abaas: Abaas,
    /// `ctx+490696`.
    pub networktest: NetworkTest,
    /// `ctx+494400`.
    pub apm: Apm,
}

impl Core {
    /// A libam that is not initialised: every module constructed (M.4), nothing configured.
    pub fn new(backend: Backend, clock: Box<dyn Clock>) -> Self {
        Self {
            exists: false,
            backend,
            clock,
            events: EventQueue::default(),
            config: AmConfig::default(),
            flow: FlowState::default(),
            location_router: false,
            master: Master::new(),
            nvram: NvramContext::new(),
            error: ErrorManager::new(),
            platform: Platform::new(),
            dongle: Dongle::new(),
            jvs: Jvs::new(),
            can: Can::new(),
            usbio: UsbIo::new(),
            appimage: AppImage::new(),
            hwm: Hwm::new(),
            app_info: AppInfoManager::new(),
            credit: Credit::new(),
            sequence: Sequence::new(),
            systemio: SystemIo::new(),
            usbdevice: UsbDevice::new(),
            video: Video::new(),
            storage: Storage::new(),
            display: Display::new(),
            network: Network::new(),
            allnet: Allnet::new(),
            laninstall: NetInstall::new(),
            aime: Aime::new(),
            emoney: EMoney::new(),
            aimepay: AimePay::new(),
            abaas: Abaas::new(),
            networktest: NetworkTest::new(),
            apm: Apm::new(),
        }
    }

    /// The context constructor again (`0x14029BBC0`): every module back to its constructed state, the backend,
    /// clock and queued events kept. `initialize` starts from here so a finalised libam starts fresh.
    pub fn construct_modules(&mut self) {
        self.config = AmConfig::default();
        self.flow = FlowState::default();
        self.location_router = false;
        self.master = Master::new();
        self.nvram = NvramContext::new();
        self.error = ErrorManager::new();
        self.platform = Platform::new();
        self.dongle = Dongle::new();
        self.jvs = Jvs::new();
        self.can = Can::new();
        self.usbio = UsbIo::new();
        self.appimage = AppImage::new();
        self.hwm = Hwm::new();
        self.app_info = AppInfoManager::new();
        self.credit = Credit::new();
        self.sequence = Sequence::new();
        self.systemio = SystemIo::new();
        self.usbdevice = UsbDevice::new();
        self.video = Video::new();
        self.storage = Storage::new();
        self.display = Display::new();
        self.network = Network::new();
        self.allnet = Allnet::new();
        self.laninstall = NetInstall::new();
        self.aime = Aime::new();
        self.emoney = EMoney::new();
        self.aimepay = AimePay::new();
        self.abaas = Abaas::new();
        self.networktest = NetworkTest::new();
        self.apm = Apm::new();
    }

    /// The local time as `struct tm`, the stamp of a raised error.
    pub fn now_tm(&self) -> Tm {
        tm_from_date_time(self.clock.local_now())
    }

    /// `0x140253370(code, aux)` (M.6): when the current error is empty the sequence play state is reset first,
    /// then the error manager takes the error (it drops it when one is already current or pending), then the
    /// idle/busy refresh runs.
    pub fn raise_error(&mut self, number: u16, sub_number: u16) {
        if self.error.current().number == 0 {
            self.sequence.reset_for_error(&mut self.events);
        }
        let time = self.now_tm();
        self.error.raise(&mut cx!(self), &mut self.master, number, sub_number, time);
        self.refresh_idle_busy();
    }

    /// `ErrorPort::reset` (`0x1402336E0`, facade variant): clears the current error when its reset type allows
    /// it, then refreshes the terminals.
    pub fn reset_error(&mut self) {
        if self.error.reset(&mut cx!(self), &mut self.master, false) {
            self.refresh_idle_busy();
        }
    }

    /// `0x140254C40`: while the process state is 3, 4 or 5, passes `idle = (processState != 4)` to ALL.Net and
    /// the ABaaS log. The enabled e-money and AimePay modules get the permit/suspend pair (`permit = idle`,
    /// `suspended = an error is current`). The Aime readers get one permit byte: idle, no error, and either no
    /// APM sub-application is set or it was started with Aime.
    pub fn refresh_idle_busy(&mut self) {
        let state = self.sequence.process_state();
        if !(3..=5).contains(&state) {
            return;
        }
        let idle = state != 4;
        let suspended = self.error.current().number != 0;
        self.allnet.refresh_idle(idle);
        self.abaas.refresh_idle(idle);
        if self.emoney.is_enabled() {
            self.emoney.set_permit(idle, suspended);
        }
        if self.aimepay.is_enabled() {
            self.aimepay.set_permit(idle, suspended);
        }
        if self.aime.enabled() {
            let readers = idle && !suspended && (self.apm.sub_game_id().is_none() || self.apm.with_aime());
            self.aime.set_permit(readers, suspended);
        }
    }

    /// The error aggregator (`0x140253D30`, M.6), run once after every main-flow step.
    ///
    /// Nothing happens while the current error is not empty, or in the test process states 2 and 5 unless
    /// `ErrorConfig.raise_in_test` is set. Otherwise the first non-empty module cell in scan order is raised;
    /// every later cell would be dropped by the error manager, so the scan stops there. The modules that are
    /// not running while the cabinet type is still loading are skipped when `in_checking`.
    pub fn aggregate_errors(&mut self, in_checking: bool) {
        if self.error.current().number != 0 {
            return;
        }
        if self.sequence.is_test() && !self.error.raise_in_test() {
            return;
        }
        let always = [
            self.platform.error_cell(),
            self.dongle.error_cell(),
            self.sequence.error_cell(),
            self.error.own_cell(),
            self.systemio.error_cell(),
            self.storage.error_cell(),
        ];
        let after_checking = |core: &Self| -> [ErrorCell; 18] {
            [
                core.hwm.error_cell(),
                core.jvs.error_cell(),
                core.can.error_cell(),
                core.usbio.error_cell(),
                core.credit.error_cell(),
                core.app_info.error_cell(),
                core.network.error_cell(),
                core.allnet.error_cell(),
                core.laninstall.error_cell(),
                core.aime.error_cell(),
                core.emoney.error_cell(),
                core.display.error_cell(),
                core.aimepay.error_cell(),
                core.abaas.error_cell(),
                core.apm.error_cell(),
                core.video.error_cell(),
                core.appimage.error_cell(),
                core.usbdevice.error_cell(),
            ]
        };
        let first = always
            .into_iter()
            .find_map(ErrorCell::get)
            .or_else(|| if in_checking { None } else { after_checking(self).into_iter().find_map(ErrorCell::get) });
        if let Some((number, sub_number)) = first {
            self.raise_error(number, sub_number);
        }
    }

    /// `allReady` (`0x14029DFB0`, `core.main.allready`): the conjunction of every module's ready term.
    pub fn all_ready(&self) -> bool {
        self.error.is_ready()
            && self.platform.is_ready()
            && self.dongle.is_done()
            && self.jvs.is_ready()
            && self.can.is_ready()
            && self.usbio.is_ready()
            && self.appimage.is_ready()
            && self.hwm.is_ready()
            && self.app_info.is_ready()
            && self.credit.is_ready()
            && self.sequence.is_ready()
            && self.usbdevice.is_ready()
            && self.video.is_ready()
            && self.storage.is_ready()
            && self.display.is_ready()
            && self.network.is_ready()
            && self.allnet.is_ready()
            && self.laninstall.is_ready()
            && self.aime.is_ready()
            && self.emoney.is_ready()
            && self.aimepay.is_ready()
            && self.abaas.is_ready()
            && self.apm.is_ready()
    }
}
