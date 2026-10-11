//! `amdaemon::server::NetDeliveryController` (vtable `0x1405D2EB8`, ctor `0x14003A890`, `App+0xC18`, size `0xC0`).
//!
//! Blocks: `ServerData.netDelivery` (`+0x370`) and `ClientData.netDelivery` (`+0x1810`)
//! ([`crate::schema::net_delivery`]). Evidence: `spec/controllers/network_install.md` §4, claims
//! `network_install.netdelivery.*`.
//!
//! The only task, `DownloadInstructionUri`, hands the string `appPath|optionPath` to libam as the local delivery
//! source; the executor acknowledges the *submission* one frame later (the checker is the constant `Succeeded`),
//! the download itself runs inside libam. Every ready frame the controller republishes libam's app and option
//! image state (times, mapped download status, version, progress) and logs a status change.

use widestring::U16CStr;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::net_delivery::{
    ClientNetDeliveryRef, NetDeliveryTask, PATH_CAPACITY, ProgressPair, ServerNetDeliveryAppInfoPatch,
    ServerNetDeliveryMut, ServerNetDeliveryOptionInfoPatch,
};
use libam_api::net_delivery::{DELIVERY_SOURCE_CAPACITY, DownloadProgress, ImageKind, NetDeliveryPort, TimeInfo};
use libam_api::result::AmError;

use crate::controllers::app_image::convert_app;
use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// libam NeedToExit reason that NetDelivery raises when a new application was downloaded (`0x140244780`).
pub const NEED_TO_EXIT_REASON: u32 = 2;

// `appPath|optionPath` fills the delivery source: two paths of `PATH_CAPACITY - 1` characters, the separator
// and the terminator.
const _: () = assert!(DELIVERY_SOURCE_CAPACITY == 2 * PATH_CAPACITY);

/// Per-image download status tracker (`0x14003E0D0`): logs when the raw status changes.
#[derive(Debug)]
struct StatusTracker {
    image: &'static str,
    last_status: i32,
}

impl StatusTracker {
    const fn new(image: &'static str) -> Self {
        Self { image, last_status: 0 }
    }

    /// Logs the unsigned status when it differs from the previous call (initially 0), then remembers it.
    fn notify(&mut self, raw_status: i32) {
        if raw_status != self.last_status {
            tracing::info!(image = self.image, status = raw_status.cast_unsigned(), "download status changed");
        }
        self.last_status = raw_status;
    }
}

/// `amdaemon::server::NetDeliveryController`.
pub struct NetDeliveryController {
    port: Box<dyn NetDeliveryPort>,
    /// App tracker at `this+0x28`.
    app_tracker: StatusTracker,
    /// Option tracker at `this+0x48`.
    option_tracker: StatusTracker,
    /// Executor at `this+0x68`; acceptor [`accept_task`], checker constant `Succeeded` (`0x14003D450`).
    executor: RequestExecutor,
}

impl std::fmt::Debug for NetDeliveryController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetDeliveryController")
            .field("app_tracker", &self.app_tracker)
            .field("option_tracker", &self.option_tracker)
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl NetDeliveryController {
    /// Constructs the controller (ctor `0x14003A890`).
    pub fn new(port: Box<dyn NetDeliveryPort>) -> Self {
        Self {
            port,
            app_tracker: StatusTracker::new("app"),
            option_tracker: StatusTracker::new("option"),
            executor: RequestExecutor::default(),
        }
    }

    /// Update steps 2–3: application times and image.
    fn update_app(&mut self, block: &mut ServerNetDeliveryMut<'_>) {
        let mut patch = ServerNetDeliveryAppInfoPatch::default();
        if let Some(times) = self.port.time_info(ImageKind::App) {
            (patch.order_time, patch.release_time) = fill_times(&times);
        }
        if let Some(image) = self.port.image_info(ImageKind::App) {
            self.app_tracker.notify(image.raw_status);
            let (version, creation_time) = convert_app(Some(&image.property));
            patch.status = Some(map_status(image.raw_status));
            patch.version = Some(version);
            patch.creation_time = Some(creation_time.0);
            patch.progress = image.progress_a.is_set().then(|| published(image.progress_a).into());
        }
        block.app_mut().copy_from(&patch).expect("scalar fields accept every value");
    }

    /// Update steps 4–5: option times and image.
    fn update_option(&mut self, block: &mut ServerNetDeliveryMut<'_>) {
        let mut patch = ServerNetDeliveryOptionInfoPatch::default();
        if let Some(times) = self.port.time_info(ImageKind::Option) {
            (patch.order_time, patch.release_time) = fill_times(&times);
        }
        if let Some(image) = self.port.image_info(ImageKind::Option) {
            let remaining = progress_difference(image.progress_a, image.progress_b);
            self.option_tracker.notify(image.raw_status);
            patch.status = Some(map_status(image.raw_status));
            patch.progress = remaining.map(|pair| published(pair).into());
            patch.progress2 = image.progress_b.is_set().then(|| published(image.progress_b).into());
        }
        block.option_mut().copy_from(&patch).expect("scalar fields accept every value");
    }
}

impl Controller for NetDeliveryController {
    fn id(&self) -> ControllerId {
        ControllerId::NetDelivery
    }

    /// `0x14003AA60`: an accepted request is awaiting its acknowledgement.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.net_delivery().generation(), ctx.server.net_delivery().response().generation())
    }

    /// `0x14003AA90`: samples `available`; nothing else is cleared.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.netdeliver_handle().is_err() {
            tracing::warn!("libam is not ready");
        } else {
            let available = self.port.is_available();
            ctx.server.net_delivery_mut().available_mut().set(available).expect("a bool accepts every value");
        }
    }

    /// `0x14003AB90`: app time, app image, option time, option image, then `checkResult`. Without libam the
    /// whole body, including the executor check, is skipped.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let Ctx { server, client, .. } = ctx;
        if self.port.netdeliver_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut block = server.net_delivery_mut();
        self.update_app(&mut block);
        self.update_option(&mut block);
        let response_generation = block.response().generation();
        let outcome = self
            .executor
            .check_result(client.net_delivery().generation(), response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut block.response_mut());
    }

    /// `0x14003AE10`: accept edge.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let client = ctx.client;
        let request = client.net_delivery();
        let response_generation = ctx.server.net_delivery().response().generation();
        let Self { port, executor, .. } = self;
        executor.accept(request.generation(), response_generation, || accept_task(port.as_mut(), &request, ctx));
    }

    /// NeedToExit callback `0x14003D470` (registered for reason 2 at `0x14003A988`): "a new application was
    /// downloaded". Raised level-triggered by libam, so it is idempotent; readiness is not consulted.
    fn on_need_to_exit(&mut self, reason: u32, ctx: &mut Ctx<'_>) {
        if reason == NEED_TO_EXIT_REASON {
            ctx.server.net_delivery_mut().exit_needed_mut().set(true).expect("a bool accepts every value");
        }
    }
}

/// `NetDeliveryController::accept` (`0x14003AE80`): `Pending` (1) when libam took the delivery source, otherwise
/// `Failed` (3).
///
/// A path without a NUL inside its 129 units makes the original's `wcscat_s` read across the adjacent field and
/// abort the process through the CRT invalid-parameter handler; here the payload fails its proof and the request
/// fails instead.
fn accept_task(port: &mut dyn NetDeliveryPort, request: &ClientNetDeliveryRef<'_>, ctx: &mut Ctx<'_>) -> RequestState {
    let task = request.task_kind();
    if ctx.manager.debug_dump() {
        tracing::info!("{}", request_trace(request, task));
    }
    if port.netdeliver_handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match task {
        Some(NetDeliveryTask::DownloadInstructionUri) => match request.download_instruction() {
            Some(paths) => port.set_local_param(&delivery_source(paths.app_path(), paths.option_path())),
            None => Err(AmError::with_message(AmError::INVALID_ARGUMENT, "Path is not NUL-terminated.")),
        },
        None => Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task.")),
    };
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

/// Request trace line (`0x14003B5C0`); the paths are only printed for the one known task, and a payload that
/// fails its proof has none to print.
fn request_trace(request: &ClientNetDeliveryRef<'_>, task: Option<NetDeliveryTask>) -> String {
    let Some(task) = task else { return "NetDelivery - task: ".to_owned() };
    let (app, option) = request.download_instruction().map_or_else(Default::default, |paths| {
        (
            String::from_utf16_lossy(paths.app_path().as_slice()),
            String::from_utf16_lossy(paths.option_path().as_slice()),
        )
    });
    format!("NetDelivery - task: {}, app: {app}, option: {option}", task.name())
}

/// The delivery source `appPath|optionPath` (`wcscat_s` x3 into a zeroed `wchar_t[258]`).
///
/// Both paths hold at most 128 characters, so the result always fits.
fn delivery_source(app: &U16CStr, option: &U16CStr) -> [u16; DELIVERY_SOURCE_CAPACITY] {
    let (app, option) = (app.as_slice(), option.as_slice());
    let mut source = [0; DELIVERY_SOURCE_CAPACITY];
    source[..app.len()].copy_from_slice(app);
    source[app.len()] = u16::from(b'|');
    source[app.len() + 1..][..option.len()].copy_from_slice(option);
    source
}

/// `mapStatus` (`0x14003B0F0`): libam's raw ALL.Net download status → the published `0..=7`. Everything not
/// listed (negative values, `4..=99`, `104..=109`, `111..=119`, `125..=129`, `131..=499`, `> 501`) is `1`.
pub const fn map_status(raw: i32) -> u32 {
    match raw {
        0 | 1 | 100 => 0,
        2 | 102 | 103 => 2,
        3 => 3,
        110 | 120 | 121 => 4,
        122..=124 => 5,
        101 | 130 => 6,
        500 | 501 => 7,
        _ => 1,
    }
}

/// `fillTimes` (`0x14003B200`): both records need a positive `tm_mday`; each valid time replaces the published
/// one, an invalid time keeps the previous value. Returns the `(order, release)` microseconds to publish, `None`
/// keeping the previous value.
fn fill_times(times: &TimeInfo) -> (Option<i64>, Option<i64>) {
    if times.order.mday <= 0 || times.release.mday <= 0 {
        return (None, None);
    }
    (times.order.to_date_time().map(|time| time.0), times.release.to_date_time().map(|time| time.0))
}

/// The libam counters as the published pair.
const fn published(pair: DownloadProgress) -> ProgressPair {
    ProgressPair { first: pair.first, second: pair.second }
}

/// `pairA - pairB` componentwise (`0x14003B2F0`, pair 1): defined only when both operands are set, and the
/// difference itself is published only when it is not the unset marker.
fn progress_difference(a: DownloadProgress, b: DownloadProgress) -> Option<DownloadProgress> {
    if !a.is_set() || !b.is_set() {
        return None;
    }
    let difference = DownloadProgress { first: a.first.wrapping_sub(b.first), second: a.second.wrapping_sub(b.second) };
    difference.is_set().then_some(difference)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::net_delivery::{ClientNetDeliveryPatch, INFO_SIZE};

    use super::*;
    use crate::controllers::controller::testing::{Harness, civil};
    use libam_api::app_image::{AppImageProperty, PropertyKind};
    use libam_api::net_delivery::ImageInfo;
    use libam_api::result::AmResult;
    use libam_api::time::Tm;

    /// libam as seen through the port.
    struct Libam {
        handle_ok: bool,
        available: bool,
        local_mode: bool,
        app_times: Option<TimeInfo>,
        option_times: Option<TimeInfo>,
        app_image: Option<ImageInfo>,
        option_image: Option<ImageInfo>,
        local_params: Vec<String>,
    }

    struct FakePort(Rc<RefCell<Libam>>);

    impl NetDeliveryPort for FakePort {
        fn netdeliver_handle(&self) -> AmResult<()> {
            if self.0.borrow().handle_ok { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
        }
        fn is_available(&self) -> bool {
            self.0.borrow().available
        }
        fn time_info(&self, kind: ImageKind) -> Option<TimeInfo> {
            let libam = self.0.borrow();
            match kind {
                ImageKind::App => libam.app_times.clone(),
                ImageKind::Option => libam.option_times.clone(),
            }
        }
        fn image_info(&self, kind: ImageKind) -> Option<ImageInfo> {
            let libam = self.0.borrow();
            match kind {
                ImageKind::App => libam.app_image.clone(),
                ImageKind::Option => libam.option_image.clone(),
            }
        }
        fn set_local_param(&mut self, source: &[u16; DELIVERY_SOURCE_CAPACITY]) -> AmResult<()> {
            let mut libam = self.0.borrow_mut();
            if !(libam.available && libam.local_mode) {
                return Err(AmError::with_message(AmError::FAILED, "unsupport function\n"));
            }
            let end = source.iter().position(|&unit| unit == 0).expect("the source is terminated");
            libam.local_params.push(String::from_utf16_lossy(&source[..end]));
            Ok(())
        }
    }

    fn fixture() -> (NetDeliveryController, Rc<RefCell<Libam>>, Harness) {
        let libam = Rc::new(RefCell::new(Libam {
            handle_ok: true,
            available: true,
            local_mode: true,
            app_times: None,
            option_times: None,
            app_image: None,
            option_image: None,
            local_params: Vec::new(),
        }));
        let controller = NetDeliveryController::new(Box::new(FakePort(Rc::clone(&libam))));
        (controller, libam, Harness::running())
    }

    /// Bytes of one path field (`wchar_t[129]`).
    const PATH_BYTES: usize = 2 * PATH_CAPACITY;

    /// A path field as the game writes it: the text's units, then zeros. A text of 129 units leaves no terminator.
    fn path_bytes(text: &str) -> [u8; PATH_BYTES] {
        let mut bytes = [0; PATH_BYTES];
        for (slot, unit) in bytes.chunks_exact_mut(2).zip(text.encode_utf16()) {
            slot.copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// The request's info bytes: the application path, then the option path.
    fn request_info(app: &[u8; PATH_BYTES], option: &[u8; PATH_BYTES]) -> [u8; INFO_SIZE] {
        let mut info = [0; INFO_SIZE];
        info[..PATH_BYTES].copy_from_slice(app);
        info[PATH_BYTES..].copy_from_slice(option);
        info
    }

    /// The client writes a request (what the IPC exchange delivers).
    fn send(h: &mut Harness, generation: u32, task: u32, info: &[u8; INFO_SIZE]) {
        let patch = ClientNetDeliveryPatch { generation: Some(generation), task: Some(task), info: Some(info) };
        h.client.net_delivery_mut().copy_from(&patch).expect("plain fields");
    }

    fn request(h: &mut Harness, generation: u32, task: u32, app: &str, option: &str) {
        send(h, generation, task, &request_info(&path_bytes(app), &path_bytes(option)));
    }

    /// Accepts the request (post), then acknowledges it (next pre).
    fn submit(controller: &mut NetDeliveryController, h: &mut Harness) {
        controller.accept_requests(&mut h.ctx());
        controller.update_server_data(&mut h.ctx());
    }

    fn tm(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Tm {
        Tm { sec, min, hour, mday, mon, year, ..Tm::default() }
    }

    fn property(version: u32, creation: Tm) -> AppImageProperty {
        AppImageProperty { kind: PropertyKind::App, initialized: true, version, option_id: [0; 5], creation }
    }

    fn pair(first: u32, second: u32) -> ProgressPair {
        ProgressPair { first, second }
    }

    fn image(raw_status: i32, progress_a: ProgressPair, progress_b: ProgressPair) -> ImageInfo {
        let libam = |pair: ProgressPair| DownloadProgress { first: pair.first, second: pair.second };
        ImageInfo {
            raw_status,
            property: property(0, tm(124, 0, 1, 0, 0, 0)),
            progress_a: libam(progress_a),
            progress_b: libam(progress_b),
        }
    }

    #[test]
    fn download_request_hands_app_pipe_option_to_libam_and_is_acknowledged() {
        let (mut controller, libam, mut h) = fixture();
        request(&mut h, 1, 0, "http://host/app.ini", "http://host/opt.ini");
        controller.accept_requests(&mut h.ctx());
        assert_eq!(libam.borrow().local_params, ["http://host/app.ini|http://host/opt.ini"]);
        assert!(controller.is_busy(&h.ctx()), "accepted but not yet acknowledged");
        assert_eq!(h.server.net_delivery().response().generation(), 0);
        controller.update_server_data(&mut h.ctx());
        assert!(h.server.net_delivery().response().succeeded());
        assert_eq!(h.server.net_delivery().response().generation(), 1);
        assert!(!controller.is_busy(&h.ctx()));
    }

    #[test]
    fn longest_paths_fill_the_258_unit_source_exactly() {
        let (mut controller, libam, mut h) = fixture();
        let app = "a".repeat(128);
        let option = "o".repeat(128);
        request(&mut h, 1, 0, &app, &option);
        submit(&mut controller, &mut h);
        assert_eq!(libam.borrow().local_params, [format!("{app}|{option}")]);
        assert!(h.server.net_delivery().response().succeeded());
    }

    #[test]
    fn path_without_terminator_fails_the_request_instead_of_overrunning() {
        for unterminated_app in [true, false] {
            let (mut controller, libam, mut h) = fixture();
            let (app, option) = if unterminated_app {
                (path_bytes(&"a".repeat(PATH_CAPACITY)), path_bytes("y"))
            } else {
                (path_bytes("x"), path_bytes(&"o".repeat(PATH_CAPACITY)))
            };
            send(&mut h, 1, 0, &request_info(&app, &option));
            submit(&mut controller, &mut h);
            assert!(libam.borrow().local_params.is_empty());
            assert!(!h.server.net_delivery().response().succeeded());
            assert_eq!(h.server.net_delivery().response().generation(), 1);
        }
    }

    #[test]
    fn libam_without_local_mode_fails_the_request() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().local_mode = false;
        request(&mut h, 3, 0, "a", "b");
        submit(&mut controller, &mut h);
        assert!(!h.server.net_delivery().response().succeeded());
        assert_eq!(h.server.net_delivery().response().generation(), 3);
    }

    #[test]
    fn ready_edge_samples_availability_and_nothing_else() {
        let (mut controller, libam, mut h) = fixture();
        let mut block = h.server.net_delivery_mut();
        block.app_mut().status_mut().set(5).expect("a u32 accepts every value");
        block.exit_needed_mut().set(true).expect("a bool accepts every value");
        controller.clear_server_data(&mut h.ctx());
        assert!(h.server.net_delivery().available());
        assert_eq!(h.server.net_delivery().app().status(), 5);
        assert!(h.server.net_delivery().exit_needed());
        libam.borrow_mut().available = false;
        controller.update_server_data(&mut h.ctx());
        assert!(h.server.net_delivery().available(), "available is a one-shot snapshot");
    }

    #[test]
    fn update_without_libam_leaves_a_pending_request_unacknowledged() {
        let (mut controller, libam, mut h) = fixture();
        request(&mut h, 1, 0, "a", "b");
        controller.accept_requests(&mut h.ctx());
        libam.borrow_mut().handle_ok = false;
        controller.update_server_data(&mut h.ctx());
        assert_eq!(h.server.net_delivery().response().generation(), 0);
        assert!(controller.is_busy(&h.ctx()));
        libam.borrow_mut().handle_ok = true;
        controller.update_server_data(&mut h.ctx());
        assert_eq!(h.server.net_delivery().response().generation(), 1);
    }

    #[test]
    fn app_image_is_republished_every_update() {
        let (mut controller, libam, mut h) = fixture();
        let mut app = image(2, pair(7, 9), pair(0, 0));
        app.property = property(0x0001_0203, tm(124, 4, 20, 12, 30, 15));
        libam.borrow_mut().app_image = Some(app);
        libam.borrow_mut().app_times =
            Some(TimeInfo { order: tm(124, 0, 2, 3, 4, 5), release: tm(124, 11, 31, 23, 59, 58) });
        controller.update_server_data(&mut h.ctx());
        let published = h.server.net_delivery().app();
        assert_eq!(published.status(), 2);
        assert_eq!(published.progress().copy_into(), pair(7, 9));
        assert_eq!(published.version(), 0x0001_0203);
        assert_eq!(published.creation_time(), civil(2024, 5, 20, 12, 30, 15).0);
        assert_eq!(published.order_time(), civil(2024, 1, 2, 3, 4, 5).0);
        assert_eq!(published.release_time(), civil(2024, 12, 31, 23, 59, 58).0);

        libam.borrow_mut().app_image = Some(image(102, pair(8, 9), pair(0, 0)));
        controller.update_server_data(&mut h.ctx());
        let published = h.server.net_delivery().app();
        assert_eq!(published.status(), 2);
        assert_eq!(published.progress().copy_into(), pair(8, 9));
        assert_eq!(published.version(), 0, "convert outputs follow the property, not the previous frame");
    }

    #[test]
    fn app_fields_keep_their_values_when_libam_has_no_data_or_unset_counters() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().app_image = Some(image(3, pair(1, 2), pair(0, 0)));
        controller.update_server_data(&mut h.ctx());
        libam.borrow_mut().app_image = None;
        controller.update_server_data(&mut h.ctx());
        let published = h.server.net_delivery().app();
        assert_eq!((published.status(), published.progress().copy_into()), (3, pair(1, 2)));
        for unset in [pair(u32::MAX, 4), pair(4, u32::MAX)] {
            libam.borrow_mut().app_image = Some(image(130, unset, pair(0, 0)));
            controller.update_server_data(&mut h.ctx());
            let published = h.server.net_delivery().app();
            assert_eq!(published.status(), 6);
            assert_eq!(published.progress().copy_into(), pair(1, 2), "{unset:?} is not published");
        }
    }

    #[test]
    fn times_need_two_positive_days_and_valid_dates_and_otherwise_keep_the_previous_value() {
        let (mut controller, libam, mut h) = fixture();
        let good = TimeInfo { order: tm(124, 4, 20, 1, 2, 3), release: tm(124, 4, 21, 1, 2, 3) };
        libam.borrow_mut().option_times = Some(good.clone());
        controller.update_server_data(&mut h.ctx());
        let first = h.server.net_delivery().option();
        let (first_order, first_release) = (first.order_time(), first.release_time());
        assert_eq!(first_order, civil(2024, 5, 20, 1, 2, 3).0);
        assert_eq!(first_release, civil(2024, 5, 21, 1, 2, 3).0);

        let zero_day = TimeInfo { order: tm(125, 0, 1, 0, 0, 0), release: tm(125, 0, 0, 0, 0, 0) };
        libam.borrow_mut().option_times = Some(zero_day);
        controller.update_server_data(&mut h.ctx());
        assert_eq!(h.server.net_delivery().option().order_time(), first_order, "tm_mday <= 0 disables both");

        let bad_release = TimeInfo { order: tm(125, 0, 1, 0, 0, 0), release: tm(125, 1, 30, 0, 0, 0) };
        libam.borrow_mut().option_times = Some(bad_release);
        controller.update_server_data(&mut h.ctx());
        assert_eq!(h.server.net_delivery().option().order_time(), civil(2025, 1, 1, 0, 0, 0).0);
        assert_eq!(h.server.net_delivery().option().release_time(), first_release, "30 February is invalid");
    }

    #[test]
    fn option_progress_is_the_difference_of_pairs_and_progress2_is_pair_b() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().option_image = Some(image(122, pair(10, 30), pair(4, 5)));
        controller.update_server_data(&mut h.ctx());
        let option = h.server.net_delivery().option();
        assert_eq!(option.status(), 5);
        assert_eq!((option.progress().copy_into(), option.progress2().copy_into()), (pair(6, 25), pair(4, 5)));

        // pairA unset: no difference, pairB still published.
        libam.borrow_mut().option_image = Some(image(122, pair(u32::MAX, u32::MAX), pair(1, 2)));
        controller.update_server_data(&mut h.ctx());
        let option = h.server.net_delivery().option();
        assert_eq!((option.progress().copy_into(), option.progress2().copy_into()), (pair(6, 25), pair(1, 2)));

        // pairB unset: neither is published.
        libam.borrow_mut().option_image = Some(image(122, pair(50, 60), pair(u32::MAX, 2)));
        controller.update_server_data(&mut h.ctx());
        let option = h.server.net_delivery().option();
        assert_eq!((option.progress().copy_into(), option.progress2().copy_into()), (pair(6, 25), pair(1, 2)));

        // A difference of -1 is indistinguishable from the unset marker and is not published either.
        libam.borrow_mut().option_image = Some(image(122, pair(3, 9), pair(4, 3)));
        controller.update_server_data(&mut h.ctx());
        let option = h.server.net_delivery().option();
        assert_eq!((option.progress().copy_into(), option.progress2().copy_into()), (pair(6, 25), pair(4, 3)));
    }

    #[test]
    fn only_need_to_exit_reason_2_sets_exit_needed() {
        for reason in [0, 1, 3] {
            let (mut controller, _libam, mut h) = fixture();
            controller.on_need_to_exit(reason, &mut h.ctx());
            assert!(!h.server.net_delivery().exit_needed(), "reason {reason}");
        }
        let (mut controller, _libam, mut h) = fixture();
        controller.on_need_to_exit(2, &mut h.ctx());
        controller.on_need_to_exit(2, &mut h.ctx());
        assert!(h.server.net_delivery().exit_needed());
        controller.clear_server_data(&mut h.ctx());
        assert!(h.server.net_delivery().exit_needed(), "there is no clearing path");
    }

    #[test]
    fn need_to_exit_is_recorded_while_the_manager_is_not_ready() {
        let (mut controller, _libam, _) = fixture();
        let mut h = Harness::new();
        assert!(!controller.is_ready(&h.ctx()));
        controller.on_need_to_exit(2, &mut h.ctx());
        assert!(h.server.net_delivery().exit_needed());
    }
}
