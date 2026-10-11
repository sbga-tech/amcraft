//! Controller construction (`App::App` `0x140001000`, steps 6 and 9).
//!
//! The 27 controllers are constructed in a fixed order which is also the registry, `preExecute`,
//! `postExecute`, busy-scan and hub-callback order and the reverse of the destruction order
//! (`spec/controllers/lifecycle.md` §3.3). Every controller owns one libam port; the three shared pointers of
//! the original (`ServerData*`, `ClientData*`, `Manager*`) and the ServerIO block are reached through
//! [`crate::controllers::controller::Ctx`] instead.

use libam_api::abaas::LogPort;
use libam_api::aime::AimePort;
use libam_api::aime_pay::AimePayPort;
use libam_api::allnet::{AccountingPort, AirPort, AuthPort, WiFiPort};
use libam_api::apm::ApmPort;
use libam_api::app_image::AppImagePort;
use libam_api::backup::BackupPort;
use libam_api::board_io::BoardIoPort;
use libam_api::can::CanPort;
use libam_api::common::CommonPort;
use libam_api::credit::CreditPort;
use libam_api::emoney::EMoneyPort;
use libam_api::error::ErrorPort;
use libam_api::input::InputPort;
use libam_api::interface::{AmInterface, ConfigLoader};
use libam_api::jvs::JvsPort;
use libam_api::lan_install::LanInstallPort;
use libam_api::net_delivery::NetDeliveryPort;
use libam_api::network::NetworkPort;
use libam_api::output::OutputPort;
use libam_api::process::ProcessPort;
use libam_api::sequence::SequencePort;
use libam_api::system_info::SystemInfoPort;
use libam_api::usb_device::UsbDevicePort;
use libam_api::usb_io::UsbIoPort;

use crate::controllers::abaas_log::LogController;
use crate::controllers::aime::AimeController;
use crate::controllers::aime_pay::AimePayController;
use crate::controllers::allnet_accounting::AccountingController;
use crate::controllers::allnet_air::AirController;
use crate::controllers::allnet_auth::AuthController;
use crate::controllers::allnet_wifi::WiFiController;
use crate::controllers::apm::ApmController;
use crate::controllers::app_image::AppImageController;
use crate::controllers::backup::{BackupController, BackupMemory};
use crate::controllers::board_io::BoardIoController;
use crate::controllers::can::CanController;
use crate::controllers::common::CommonController;
use crate::controllers::controller::Controller;
use crate::controllers::credit::CreditController;
use crate::controllers::emoney::EMoneyController;
use crate::controllers::error::ErrorController;
use crate::controllers::input::InputController;
use crate::controllers::jvs::JvsController;
use crate::controllers::lan_install::LanInstallController;
use crate::controllers::net_delivery::NetDeliveryController;
use crate::controllers::network::NetworkController;
use crate::controllers::output::OutputController;
use crate::controllers::process::ProcessController;
use crate::controllers::sequence::SequenceController;
use crate::controllers::system_info::SystemInfoController;
use crate::controllers::usb_device::UsbDeviceController;
use crate::controllers::usb_io::UsbIoController;
use crate::daemon::app::{AppServices, ServerExchange, SystemServices};
use crate::daemon::log::LogPath;

/// One libam port per controller, in construction order.
pub struct ControllerPorts {
    /// #1 `ProcessController`.
    pub process: Box<dyn ProcessPort>,
    /// #2 `CommonController`.
    pub common: Box<dyn CommonPort>,
    /// #3 `SystemInfoController`.
    pub system_info: Box<dyn SystemInfoPort>,
    /// #4 `ErrorController`.
    pub error: Box<dyn ErrorPort>,
    /// #5 `SequenceController`.
    pub sequence: Box<dyn SequencePort>,
    /// #6 `JvsController`.
    pub jvs: Box<dyn JvsPort>,
    /// #7 `CanController`.
    pub can: Box<dyn CanPort>,
    /// #8 `UsbIOController`.
    pub usb_io: Box<dyn UsbIoPort>,
    /// #9 `InputController`.
    pub input: Box<dyn InputPort>,
    /// #10 `OutputController`.
    pub output: Box<dyn OutputPort>,
    /// #11 `BackupController`.
    pub backup: Box<dyn BackupPort>,
    /// #12 `BoardIoController`.
    pub board_io: Box<dyn BoardIoPort>,
    /// #13 `UsbDeviceController`.
    pub usb_device: Box<dyn UsbDevicePort>,
    /// #14 `AppImageController`.
    pub app_image: Box<dyn AppImagePort>,
    /// #15 `CreditController`.
    pub credit: Box<dyn CreditPort>,
    /// #16 `NetworkController`.
    pub network: Box<dyn NetworkPort>,
    /// #17 `LanInstallController`.
    pub lan_install: Box<dyn LanInstallPort>,
    /// #18 `NetDeliveryController`.
    pub net_delivery: Box<dyn NetDeliveryPort>,
    /// #19 `allnet::AuthController`.
    pub allnet_auth: Box<dyn AuthPort>,
    /// #20 `allnet::AccountingController`.
    pub allnet_accounting: Box<dyn AccountingPort>,
    /// #21 `allnet::WiFiController`.
    pub allnet_wifi: Box<dyn WiFiPort>,
    /// #22 `allnet::AirController`.
    pub allnet_air: Box<dyn AirPort>,
    /// #23 `AimeController`.
    pub aime: Box<dyn AimePort>,
    /// #24 `EMoneyController`.
    pub emoney: Box<dyn EMoneyPort>,
    /// #25 `AimePayController`.
    pub aime_pay: Box<dyn AimePayPort>,
    /// #26 `abaas::LogController`.
    pub abaas_log: Box<dyn LogPort>,
    /// #27 `ApmController`.
    pub apm: Box<dyn ApmPort>,
}

impl std::fmt::Debug for ControllerPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerPorts").finish_non_exhaustive()
    }
}

impl ControllerPorts {
    /// Every port from one libam: the handles share its state, as the facades of the original share
    /// the statically linked library.
    pub fn from_libam(libam: &libam::Libam) -> Self {
        Self {
            process: libam.process(),
            common: libam.common(),
            system_info: libam.system_info(),
            error: libam.error(),
            sequence: libam.sequence(),
            jvs: libam.jvs(),
            can: libam.can(),
            usb_io: libam.usb_io(),
            input: libam.input(),
            output: libam.output(),
            backup: libam.backup(),
            board_io: libam.board_io(),
            usb_device: libam.usb_device(),
            app_image: libam.app_image(),
            credit: libam.credit(),
            network: libam.network(),
            lan_install: libam.lan_install(),
            net_delivery: libam.net_delivery(),
            allnet_auth: libam.auth(),
            allnet_accounting: libam.accounting(),
            allnet_wifi: libam.wifi(),
            allnet_air: libam.air(),
            aime: libam.aime(),
            emoney: libam.emoney(),
            aime_pay: libam.aime_pay(),
            abaas_log: libam.log(),
            apm: libam.apm(),
        }
    }
}

/// Constructs the 27 controllers in construction order (`ControllerId::ORDER`).
///
/// `backup_memory` is the `ipc::Backup` object the Backup controller owns (`+0x88`) and `relation_index` the IPC
/// relation index it stores (`BackupController` 5th constructor argument, `0x14006BD00`).
pub fn build_controllers(
    ports: ControllerPorts,
    backup_memory: Box<dyn BackupMemory>,
    relation_index: u32,
) -> Vec<Box<dyn Controller>> {
    vec![
        Box::new(ProcessController::new(ports.process)),
        Box::new(CommonController::new(ports.common)),
        Box::new(SystemInfoController::new(ports.system_info)),
        Box::new(ErrorController::new(ports.error)),
        Box::new(SequenceController::new(ports.sequence)),
        Box::new(JvsController::new(ports.jvs)),
        Box::new(CanController::new(ports.can)),
        Box::new(UsbIoController::new(ports.usb_io)),
        Box::new(InputController::new(ports.input)),
        Box::new(OutputController::new(ports.output)),
        Box::new(BackupController::new(ports.backup, backup_memory, relation_index)),
        Box::new(BoardIoController::new(ports.board_io)),
        Box::new(UsbDeviceController::new(ports.usb_device)),
        Box::new(AppImageController::new(ports.app_image)),
        Box::new(CreditController::new(ports.credit)),
        Box::new(NetworkController::new(ports.network)),
        Box::new(LanInstallController::new(ports.lan_install)),
        Box::new(NetDeliveryController::new(ports.net_delivery)),
        Box::new(AuthController::new(ports.allnet_auth)),
        Box::new(AccountingController::new(ports.allnet_accounting)),
        Box::new(WiFiController::new(ports.allnet_wifi)),
        Box::new(AirController::new(ports.allnet_air)),
        Box::new(AimeController::new(ports.aime)),
        Box::new(EMoneyController::new(ports.emoney)),
        Box::new(AimePayController::new(ports.aime_pay)),
        Box::new(LogController::new(ports.abaas_log)),
        Box::new(ApmController::new(ports.apm)),
    ]
}

impl AppServices {
    /// Services with the real controllers: the Backup controller receives the server's relation index
    /// (`Server::Impl+0x1C078` → `0x14006BD00`), which is why the server must exist before the controllers.
    pub fn with_ports(
        system: SystemServices,
        server: Box<dyn ServerExchange>,
        ports: ControllerPorts,
        backup_memory: Box<dyn BackupMemory>,
        interface: Box<dyn AmInterface>,
        config_loader: Box<dyn ConfigLoader>,
        log: LogPath,
    ) -> Self {
        let controllers = build_controllers(ports, backup_memory, server.relation_index());
        Self { system, server, controllers, interface, config_loader, log }
    }

    /// The production composition: every controller port, the interface and the configuration loader from one
    /// libam.
    pub fn with_libam(
        system: SystemServices,
        server: Box<dyn ServerExchange>,
        libam: &libam::Libam,
        backup_memory: Box<dyn BackupMemory>,
        log: LogPath,
    ) -> Self {
        let ports = ControllerPorts::from_libam(libam);
        Self::with_ports(system, server, ports, backup_memory, libam.interface(), libam.config_loader(), log)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    use backend::mock::ipc::LoopbackPlatform;

    use super::*;
    use crate::daemon::app::App;
    use crate::daemon::args::ArgumentsParser;
    use crate::daemon::backup::IpcBackupMemory;
    use crate::daemon::window::MessagePump;
    use crate::ipc::server::Server;
    use crate::mock::{self, FakeClock, FakeConsole, FakeWindow, Recorded, Seen};

    /// The daemon's main loop with every controller port, the interface and the configuration loader from the
    /// real libam on a simulated healthy cabinet: libam asks for the cabinet type, the daemon answers from the
    /// `config.json` next to the executable (whose defaults name the `am_resources` folder beside it and the
    /// Japanese resource set), libam enters Operating (it opens the application file of the keychip's game id)
    /// and moves the sequence to advertise, and no fatal error is raised.
    #[test]
    fn the_daemon_runs_its_frames_through_libam_on_a_simulated_cabinet() {
        let root = tempfile::tempdir().expect("temporary directory");
        let exe_dir = root.path().join("exe");
        let resources = exe_dir.join("am_resources");
        fs::create_dir_all(&resources).expect("resource directory");
        fs::write(exe_dir.join("config.json"), r#"{"common": {"max_player": 2}}"#).expect("config");
        fs::write(resources.join("error.japanese.json"), "[]").expect("error resource");
        fs::write(resources.join("warning.japanese.json"), "[]").expect("warning resource");
        let module_path = exe_dir.join("amdaemon.exe").display().to_string();

        let cabinet = root.path().join("cabinet");
        let (backend, sim) = libam::mock::SimBackend::new(&cabinet);
        let libam = libam::Libam::new(backend, Box::new(sim.clock.clone()));
        let events = Rc::new(RefCell::new(Vec::new()));
        let interface = Recorded { inner: libam.interface(), events: Rc::clone(&events) };
        let trace = mock::new_trace();
        let mut window = FakeWindow::new(&trace);
        window.pump.extend(std::iter::repeat_n(MessagePump::Idle, 120));
        window.pump.push_back(MessagePump::Quit);
        let system = SystemServices {
            clock: Box::new(FakeClock::new(&trace)),
            window: Box::new(window),
            console: Box::new(FakeConsole::new(&trace)),
        };
        let ipc = LoopbackPlatform::new();
        let services = AppServices::with_ports(
            system,
            Box::new(Server::new(Box::new(ipc.clone()))),
            ControllerPorts::from_libam(&libam),
            Box::new(IpcBackupMemory::new(Box::new(ipc))),
            Box::new(interface),
            libam.config_loader(),
            LogPath::new(&module_path),
        );
        let mut app = App::new(services).expect("27 controllers in registry order");
        let mut arguments = ArgumentsParser::new();
        arguments.parse(["amdaemon.exe"], &module_path).expect("no options");
        assert_eq!(app.run(&arguments), 0);

        let events = events.borrow();
        let requested = events.iter().position(|event| *event == Seen::CabinetTypeRequest);
        let advertise = events.iter().position(|event| *event == Seen::ChangeState(3));
        assert!(requested.is_some() && advertise > requested, "cabinet type, then advertise: {events:?}");
        // Number 0 is the empty item the error manager announces when it has no error to show.
        assert!(
            !events.iter().any(|event| matches!(event, Seen::FatalError(number) if *number != 0)),
            "no fatal error on a healthy cabinet: {events:?}"
        );
        assert!(cabinet.join("c").join("appdata").join("SDAA").join("appfile.dat").is_file());
    }
}
