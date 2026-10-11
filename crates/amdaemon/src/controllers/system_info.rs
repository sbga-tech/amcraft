//! `amdaemon::server::SystemInfoController` (`App+0x588`, vtable `0x1405DEA08`).
//!
//! SystemInfo copies libam's board, dongle, video and storage facts into the `systemInfo` block each time
//! the controller becomes ready, and refreshes the application root and game id whenever the application
//! state changes (`spec/controllers/process_common_systeminfo.md` §3). It has no executor, is never busy and
//! does not accept requests.

use libam_api::system_info::{LibamSerial, SystemInfoPort};

use crate::schema::system_info::{Resolution, SerialIdMut, SerialIdPatch, ServerSystemInfoPatch};
use crate::schema::text::fit_wide;

use crate::controllers::controller::{Controller, ControllerId, Ctx};

/// `amdaemon::server::SystemInfoController`.
pub struct SystemInfoController {
    port: Box<dyn SystemInfoPort>,
}

impl std::fmt::Debug for SystemInfoController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemInfoController").finish_non_exhaustive()
    }
}

impl SystemInfoController {
    /// Constructs the controller (`0x140046DC0`). The APM state listener the original registers here is
    /// [`Controller::on_app_state`].
    pub fn new(port: Box<dyn SystemInfoPort>) -> Self {
        Self { port }
    }

    /// `getBoardInfo` (`0x140046EB0`): region first, independent of the serial.
    fn get_board_info(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.board_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut info = ctx.server.system_info_mut();
        // Only the libam region values 1, 2 and 3 are published; anything else (or absent) is 0.
        let region = self.port.board_region().filter(|region| (1..=3).contains(region)).unwrap_or(0);
        info.region_code_mut().set(region).expect("u32 accepts every value");
        match self.port.board_serial() {
            Some(serial) => publish_serial(&mut info.board_id_mut(), &serial),
            None => {
                tracing::error!("broken system data");
                info.board_id_mut().clear();
            }
        }
    }

    /// `getGameId` (`0x140047860`): the apm sub-game id wins over the dongle game id.
    fn get_game_id(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.dongle_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        if self.port.apm_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let game_id = self.port.apm_game_id().or_else(|| self.port.dongle_game_id());
        if game_id.is_none() {
            tracing::error!("dongle is disconnected");
        }
        let mut info = ctx.server.system_info_mut();
        let mut field = info.game_id_mut();
        let fitted = fit_wide(game_id.as_ref().map(|id| id.as_slice()).unwrap_or_default(), field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }

    /// `getDongleInfo` (`0x140047160`): model type and develop byte are written even without a keychip.
    fn get_dongle_info(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.dongle_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut info = ctx.server.system_info_mut();
        match self.port.keychip_serial() {
            Some(serial) => publish_serial(&mut info.keychip_id_mut(), &serial),
            None => {
                tracing::error!("dongle is disconnected");
                info.keychip_id_mut().clear();
            }
        }
        let patch = ServerSystemInfoPatch {
            model_type: Some(self.port.model_type().unwrap_or(2)),
            develop: Some(self.port.develop()),
            ..ServerSystemInfoPatch::default()
        };
        info.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `getVideoInfo` (`0x140047400`): descriptor 1 fills slot 0, descriptor 0 fills slot 1.
    fn get_video_info(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.video_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let port = &mut self.port;
        let slots = [1, 0].map(|descriptor| {
            let resolution = port.video_resolution(descriptor);
            (Resolution { width: resolution.width, height: resolution.height }, port.video_output_active(descriptor))
        });
        let patch = ServerSystemInfoPatch {
            resolutions: Some(slots.map(|(resolution, _)| resolution)),
            output_status: Some(slots.map(|(_, active)| active)),
            ..ServerSystemInfoPatch::default()
        };
        ctx.server.system_info_mut().copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `getAppRootPathOnStorage` (`0x140047580`).
    fn get_app_root_path_on_storage(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.storage_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let path = self.port.app_root_path();
        if path.is_none() {
            tracing::error!("dongle is disconnected");
        }
        let mut info = ctx.server.system_info_mut();
        let mut field = info.app_root_path_mut();
        let fitted = fit_wide(path.as_ref().map(|path| path.as_slice()).unwrap_or_default(), field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }

    /// `getRamDiskPath` (`0x1400476F0`).
    fn get_ram_disk_path(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.storage_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let path = self.port.ram_disk_path();
        if path.is_none() {
            tracing::error!("broken system data");
        }
        let mut info = ctx.server.system_info_mut();
        let mut field = info.ram_disk_path_mut();
        let fitted = fit_wide(path.as_ref().map(|path| path.as_slice()).unwrap_or_default(), field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }
}

impl Controller for SystemInfoController {
    fn id(&self) -> ControllerId {
        ControllerId::SystemInfo
    }

    /// `0x140046E60`: populate the whole block on each not-ready → ready edge, in this exact order.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.get_board_info(ctx);
        self.get_game_id(ctx);
        self.get_dongle_info(ctx);
        self.get_video_info(ctx);
        self.get_app_root_path_on_storage(ctx);
        self.get_ram_disk_path(ctx);
    }

    /// APM listener lambda (`0x140047EA0`): refreshes only the application root and the game id, whatever the
    /// state value and whether or not the controller is ready.
    fn on_app_state(&mut self, _state: u32, ctx: &mut Ctx<'_>) {
        self.get_app_root_path_on_storage(ctx);
        self.get_game_id(ctx);
    }
}

/// `makeSerialId<StandardSerialId>` (`0x140063A80`) and `makeSerialId<ShortSerialId>` (`0x140063990`): an
/// all-zero id into which each part is copied with `wcsncpy_s(dst, 17, src, 16)` and `wcsncpy_s(dst, 12, src, 11)`,
/// so over-long sources are silently truncated. The controller assigns the whole id to the block, so the previous
/// id is cleared first.
fn publish_serial(id: &mut SerialIdMut<'_>, serial: &LibamSerial) {
    let standard = fit_wide(&serial.standard, id.standard_mut().capacity());
    let short = fit_wide(&serial.short, id.short_mut().capacity());
    let patch = SerialIdPatch { standard: Some(&standard), short: Some(&short) };
    id.clear();
    id.copy_from(&patch).expect("fitted to the fields");
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use libam_api::result::{AmError, AmResult};
    use libam_api::sequence::ProcessState;
    use libam_api::system_info::VideoResolution;
    use widestring::U16CStr;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, buffer};

    // `ServerData.systemInfo` (`+0x9050`, `0x49C` bytes) and the fields read raw below.
    const BLOCK: usize = 0x9050;
    const SIZE: usize = 0x49C;
    const PAD_03A: usize = 0x03A;
    const KEYCHIP_ID: usize = 0x040;
    const PAD_07B: usize = 0x07B;
    const PAD_087: usize = 0x087;
    const APP_ROOT_PATH: usize = 0x098;
    const PAD_49A: usize = 0x49A;

    /// Everything libam can be asked for, switchable per test.
    #[derive(Clone)]
    struct Sources {
        board_facade: bool,
        dongle_facade: bool,
        video_facade: bool,
        storage_facade: bool,
        apm_facade: bool,
        region: Option<u32>,
        board_serial: Option<LibamSerial>,
        keychip_serial: Option<LibamSerial>,
        model_type: Option<u8>,
        develop: u8,
        dongle_game_id: Option<Vec<u16>>,
        apm_game_id: Option<Vec<u16>>,
        /// Indexed by libam descriptor index.
        video: [(VideoResolution, bool); 2],
        app_root: Option<Vec<u16>>,
        ram_disk: Option<Vec<u16>>,
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// `text` as a NUL-terminated libam string array (at most `N - 1` units).
    fn padded<const N: usize>(text: &str) -> [u16; N] {
        let mut array = [0; N];
        for (unit, source) in array.iter_mut().zip(text.encode_utf16().take(N - 1)) {
            *unit = source;
        }
        array
    }

    fn serial(standard: &str, short: &str) -> LibamSerial {
        LibamSerial { standard: padded(standard), short: padded(short) }
    }

    fn healthy() -> Sources {
        Sources {
            board_facade: true,
            dongle_facade: true,
            video_facade: true,
            storage_facade: true,
            apm_facade: true,
            region: Some(2),
            board_serial: Some(serial("BOARD-0123456789", "BRD-0123456")),
            keychip_serial: Some(serial("KEYCHIP-ABCDEFGH", "KEY-ABCDEFG")),
            model_type: Some(3),
            develop: 1,
            dongle_game_id: Some(units("SBYZ")),
            apm_game_id: None,
            video: [
                (VideoResolution { width: 1920, height: 1080 }, true),
                (VideoResolution { width: 1280, height: 720 }, false),
            ],
            app_root: Some(units(r"D:\game\")),
            ram_disk: Some(units(r"R:\")),
        }
    }

    /// Port double reading the shared [`Sources`].
    struct TestPort {
        sources: Rc<RefCell<Sources>>,
    }

    fn facade(ok: bool) -> AmResult<()> {
        if ok { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
    }

    impl SystemInfoPort for TestPort {
        fn board_facade(&mut self) -> AmResult<()> {
            facade(self.sources.borrow().board_facade)
        }
        fn dongle_facade(&mut self) -> AmResult<()> {
            facade(self.sources.borrow().dongle_facade)
        }
        fn video_facade(&mut self) -> AmResult<()> {
            facade(self.sources.borrow().video_facade)
        }
        fn storage_facade(&mut self) -> AmResult<()> {
            facade(self.sources.borrow().storage_facade)
        }
        fn apm_facade(&mut self) -> AmResult<()> {
            facade(self.sources.borrow().apm_facade)
        }
        fn board_region(&mut self) -> Option<u32> {
            self.sources.borrow().region
        }
        fn board_serial(&mut self) -> Option<LibamSerial> {
            self.sources.borrow().board_serial
        }
        fn keychip_serial(&mut self) -> Option<LibamSerial> {
            self.sources.borrow().keychip_serial
        }
        fn model_type(&mut self) -> Option<u8> {
            self.sources.borrow().model_type
        }
        fn develop(&mut self) -> u8 {
            self.sources.borrow().develop
        }
        fn dongle_game_id(&mut self) -> Option<[u16; 5]> {
            self.sources.borrow().dongle_game_id.as_deref().map(buffer)
        }
        fn apm_game_id(&mut self) -> Option<[u16; 5]> {
            self.sources.borrow().apm_game_id.as_deref().map(buffer)
        }
        fn video_resolution(&mut self, index: u32) -> VideoResolution {
            self.sources.borrow().video[index as usize].0
        }
        fn video_output_active(&mut self, index: u32) -> bool {
            self.sources.borrow().video[index as usize].1
        }
        fn app_root_path(&mut self) -> Option<[u16; 260]> {
            self.sources.borrow().app_root.as_deref().map(buffer)
        }
        fn ram_disk_path(&mut self) -> Option<[u16; 260]> {
            self.sources.borrow().ram_disk.as_deref().map(buffer)
        }
    }

    fn sources_with(edit: impl FnOnce(&mut Sources)) -> Rc<RefCell<Sources>> {
        let mut sources = healthy();
        edit(&mut sources);
        Rc::new(RefCell::new(sources))
    }

    fn slot(sources: &Rc<RefCell<Sources>>) -> ControllerSlot {
        let port = TestPort { sources: Rc::clone(sources) };
        ControllerSlot::new(Box::new(SystemInfoController::new(Box::new(port))))
    }

    /// First tick of a Running Manager: the not-ready → ready edge populates the block.
    fn populated(sources: &Rc<RefCell<Sources>>) -> (Harness, ControllerSlot) {
        let mut slot = slot(sources);
        let mut harness = Harness::running();
        slot.pre_execute(&mut harness.ctx());
        (harness, slot)
    }

    /// The raw bytes of the `systemInfo` block.
    fn block(harness: &Harness) -> &[u8] {
        &harness.server.bytes()[BLOCK..BLOCK + SIZE]
    }

    fn is_zero(bytes: &[u8]) -> bool {
        bytes.iter().all(|&byte| byte == 0)
    }

    fn text(string: &U16CStr) -> String {
        String::from_utf16_lossy(string.as_slice())
    }

    #[test]
    fn ready_edge_publishes_every_field_from_libam() {
        let sources = sources_with(|_| {});
        let (harness, _slot) = populated(&sources);
        let info = harness.server.system_info();

        assert_eq!(text(info.board_id().standard()), "BOARD-0123456789");
        assert_eq!(text(info.board_id().short()), "BRD-0123456");
        assert_eq!(info.region_code(), 2);
        assert_eq!(text(info.keychip_id().standard()), "KEYCHIP-ABCDEFGH");
        assert_eq!(text(info.keychip_id().short()), "KEY-ABCDEFG");
        assert_eq!(info.model_type(), 3);
        assert_eq!(text(info.game_id()), "SBYZ");
        assert_eq!(info.develop(), 1);
        assert_eq!(text(info.app_root_path()), r"D:\game\");
        assert_eq!(text(info.ram_disk_path()), r"R:\");
        assert!(is_zero(&block(&harness)[PAD_03A..PAD_03A + 2]));
        assert!(is_zero(&block(&harness)[PAD_07B..PAD_07B + 1]));
        assert!(is_zero(&block(&harness)[PAD_087..PAD_087 + 1]));
        assert!(is_zero(&block(&harness)[PAD_49A..PAD_49A + 2]));
    }

    #[test]
    fn video_descriptor_one_fills_slot_zero_and_descriptor_zero_slot_one() {
        let sources = sources_with(|_| {});
        let (harness, _slot) = populated(&sources);
        let info = harness.server.system_info();

        let resolutions = info.resolutions();
        assert_eq!(resolutions.get(0).expect("slot 0").copy_into(), Resolution { width: 1280, height: 720 });
        assert_eq!(resolutions.get(1).expect("slot 1").copy_into(), Resolution { width: 1920, height: 1080 });
        assert_eq!(info.output_status().copy_into(), [false, true]);
    }

    #[test]
    fn block_is_written_on_ready_edges_only() {
        let sources = sources_with(|_| {});
        let mut slot = slot(&sources);
        let mut harness = Harness::new();

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(block(&harness).iter().copied().max(), Some(0), "nothing before ready");

        harness.manager.on_change_state(ProcessState(3));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(harness.server.system_info().region_code(), 2);

        sources.borrow_mut().region = Some(3);
        slot.pre_execute(&mut harness.ctx());
        slot.post_execute(&mut harness.ctx());
        assert_eq!(harness.server.system_info().region_code(), 2, "no periodic refresh");

        // Running -> Setup -> Running is a new ready edge.
        harness.manager.on_change_state(ProcessState(1));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(harness.server.system_info().region_code(), 2);
        harness.manager.on_change_state(ProcessState(4));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(harness.server.system_info().region_code(), 3);
    }

    #[test]
    fn only_libam_regions_one_to_three_are_published() {
        for (libam, published) in [(Some(0), 0), (Some(1), 1), (Some(2), 2), (Some(3), 3), (Some(4), 0), (None, 0)] {
            let sources = sources_with(|s| s.region = libam);
            let (harness, _slot) = populated(&sources);
            assert_eq!(harness.server.system_info().region_code(), published, "libam region {libam:?}");
        }
    }

    #[test]
    fn serials_are_cut_to_sixteen_and_eleven_units() {
        let sources = sources_with(|s| {
            s.board_serial = Some(LibamSerial { standard: [u16::from(b'A'); 17], short: [u16::from(b'B'); 12] });
        });
        let (harness, _slot) = populated(&sources);
        let info = harness.server.system_info();
        let id = info.board_id();

        // A published string is always terminated within its field, so the length is the cut.
        assert_eq!(id.standard().len(), 16);
        assert_eq!(id.short().len(), 11);
    }

    #[test]
    fn missing_keychip_still_publishes_model_and_develop() {
        let sources = sources_with(|s| {
            s.keychip_serial = None;
            s.model_type = None;
            s.develop = 0xFE;
        });
        let mut slot = slot(&sources);
        let mut harness = Harness::running();
        publish_serial(&mut harness.server.system_info_mut().keychip_id_mut(), &serial("STALE", "OLD"));

        slot.pre_execute(&mut harness.ctx());

        assert!(is_zero(&block(&harness)[KEYCHIP_ID..KEYCHIP_ID + 0x3A]), "the whole keychip id is zero");
        let info = harness.server.system_info();
        assert_eq!(info.model_type(), 2, "absent model type defaults to 2");
        assert_eq!(info.develop(), 0xFE, "the develop byte is copied raw");
    }

    #[test]
    fn game_id_prefers_the_apm_sub_game_and_is_cut_to_four_units() {
        let sources = sources_with(|s| s.apm_game_id = Some(units("SDEZZ")));
        let (harness, _slot) = populated(&sources);

        assert_eq!(text(harness.server.system_info().game_id()), "SDEZ");
    }

    #[test]
    fn paths_are_cut_to_255_units() {
        let long = "p".repeat(300);
        let sources = sources_with(|s| {
            s.app_root = Some(units(&long));
            s.ram_disk = Some(units(&long));
        });
        let (harness, _slot) = populated(&sources);
        let info = harness.server.system_info();

        assert_eq!(info.app_root_path().len(), 255);
        assert_eq!(info.ram_disk_path().len(), 255);
    }

    #[test]
    fn missing_facades_write_nothing() {
        let sources = sources_with(|_| {});
        let (mut harness, mut slot) = populated(&sources);
        let before = block(&harness).to_vec();
        // Every answer differs from the populated block, so any write would show.
        *sources.borrow_mut() = Sources {
            board_facade: false,
            dongle_facade: false,
            video_facade: false,
            storage_facade: false,
            apm_facade: false,
            region: Some(3),
            board_serial: Some(serial("OTHER-BOARD", "OTHER")),
            keychip_serial: Some(serial("OTHER-KEYCHIP", "OTHER")),
            model_type: Some(1),
            develop: 0,
            dongle_game_id: Some(units("XXXX")),
            apm_game_id: Some(units("YYYY")),
            video: [(VideoResolution { width: 1, height: 2 }, false), (VideoResolution { width: 3, height: 4 }, true)],
            app_root: Some(units(r"Z:\")),
            ram_disk: Some(units(r"Y:\")),
        };

        slot.controller_mut().clear_server_data(&mut harness.ctx());

        assert_eq!(block(&harness), before);
    }

    #[test]
    fn shorter_values_leave_the_old_tail_behind_the_terminator() {
        let sources = sources_with(|s| s.app_root = Some(units(r"D:\a-long-path\")));
        let (mut harness, mut slot) = populated(&sources);

        sources.borrow_mut().app_root = Some(units("X:"));
        slot.controller_mut().on_app_state(0, &mut harness.ctx());

        let root: Vec<u16> = block(&harness)[APP_ROOT_PATH..APP_ROOT_PATH + 2 * 256]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(&root[..3], &units("X:\0")[..]);
        assert_eq!(&root[3..6], &units("a-l")[..], "wcsncpy_s does not clear the rest of the destination");
    }

    #[test]
    fn app_state_listener_refreshes_only_the_app_root_and_the_game_id() {
        let sources = sources_with(|_| {});
        let (mut harness, mut slot) = populated(&sources);
        {
            let mut sources = sources.borrow_mut();
            sources.app_root = Some(units(r"E:\next\"));
            sources.apm_game_id = Some(units("SDGA"));
            sources.ram_disk = Some(units(r"Q:\"));
            sources.region = Some(1);
            sources.keychip_serial = None;
        }

        slot.controller_mut().on_app_state(7, &mut harness.ctx());

        let info = harness.server.system_info();
        assert_eq!(text(info.app_root_path()), r"E:\next\");
        assert_eq!(text(info.game_id()), "SDGA");
        assert_eq!(text(info.ram_disk_path()), r"R:\");
        assert_eq!(info.region_code(), 2);
        assert_eq!(text(info.keychip_id().standard()), "KEYCHIP-ABCDEFGH");
    }

    #[test]
    fn app_state_listener_does_not_wait_for_readiness() {
        let sources = sources_with(|_| {});
        let mut slot = slot(&sources);
        let mut harness = Harness::new();

        slot.controller_mut().on_app_state(0, &mut harness.ctx());

        let info = harness.server.system_info();
        assert_eq!(text(info.app_root_path()), r"D:\game\");
        assert_eq!(text(info.game_id()), "SBYZ");
        assert_eq!(info.region_code(), 0, "the rest waits for the ready edge");
    }
}
