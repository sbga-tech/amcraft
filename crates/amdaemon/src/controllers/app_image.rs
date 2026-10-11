//! `amdaemon::server::AppImageController` (vtable `0x1405C4488`): publishes the installed app image, the
//! installed option images and the option root directory from libam's `am::appimage::Manager` list.
//!
//! The controller has no request block and no executor. It loads everything once on the ready rising edge and
//! reloads the app and option properties (not the root path) on every application-state notification.
//!
//! Evidence: `spec/controllers/backup_appimage.md` §5 (claim ids `backup_appimage.appimage.*`).

use crate::schema::app_image::{MAX_OPTIONS, OptionImageInfoMut, ServerAppImageMut, ServerAppImagePatch};
use crate::schema::text::fit_wide;
use libam::appimage::{AppImagePort, AppImageProperty, PropertyKind};
use libam::time::DateTime;

use crate::controllers::controller::{Controller, ControllerId, Ctx};

/// `AppImageProperty::convert(prop, Version&, DateTime&)` (`0x140051EE0`): the version and creation time of an
/// App/OS property, `(0, DateTime(0))` for a null or invalid one.
///
/// A bad timestamp or an option property in an app slot is logged and the outputs stay zero. Also used by the
/// NetDelivery controller.
pub fn convert_app(property: Option<&AppImageProperty>) -> (u32, DateTime) {
    let zero = (0, DateTime(0));
    let Some(property) = property.filter(|property| property.is_valid()) else {
        return zero;
    };
    let Some(created) = property.creation.to_date_time() else {
        tracing::error!("invalid creation timestamp");
        return zero;
    };
    if property.kind == PropertyKind::Option {
        tracing::warn!("invalid property type");
        return zero;
    }
    (property.version, created)
}

/// `AppImageProperty::convert(prop, wchar_t(&)[5], DateTime&)` (`0x140052040`): the id and creation time of an
/// option property, `([0; 5], DateTime(0))` for a null or invalid one.
///
/// The original first "clears" the entry's id like `wcsncpy_s(name, 5, L"", 4)`, which only terminates it at the
/// first unit; [`publish_option`] reproduces that by cutting a blank id to its terminator.
fn convert_option(property: Option<&AppImageProperty>) -> ([u16; 5], DateTime) {
    let blank = ([0; 5], DateTime(0));
    let Some(property) = property.filter(|property| property.is_valid()) else {
        return blank;
    };
    let Some(created) = property.creation.to_date_time() else {
        tracing::error!("invalid creation timestamp");
        return blank;
    };
    if property.kind != PropertyKind::Option {
        tracing::warn!("invalid property type");
        return blank;
    }
    (property.option_id, created)
}

/// Writes a converted option into its entry: `wcsncpy_s(name, 5, id, 4)` keeps the units behind the terminator,
/// so a blank id leaves the tail of the previous name in place.
fn publish_option(entry: &mut OptionImageInfoMut<'_>, id: &[u16; 5], created: DateTime) {
    let mut name = entry.name_mut();
    let fitted = fit_wide(id, name.capacity());
    name.set(&fitted).expect("fitted to the field");
    entry.creation_time_mut().set(created.0).expect("scalar fields accept every value");
}

/// `amdaemon::server::AppImageController`.
pub struct AppImageController {
    port: Box<dyn AppImagePort>,
}

impl std::fmt::Debug for AppImageController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppImageController").finish_non_exhaustive()
    }
}

impl AppImageController {
    /// Constructs the controller (`0x14001FA20`). The original also registers the APM-state listener;
    /// here [`Controller::on_app_state`] is that listener.
    pub fn new(port: Box<dyn AppImagePort>) -> Self {
        Self { port }
    }

    /// `loadAppProperty` (`0x14001FAF0`): an unavailable libam publishes version 0 and `DateTime(0)`.
    fn load_app_property(&self, block: &mut ServerAppImageMut<'_>) {
        let (version, created) = if self.port.acquire_app_image().is_ok() {
            convert_app(self.port.current_app().as_ref())
        } else {
            tracing::warn!("libam is not ready");
            (0, DateTime(0))
        };
        let patch = ServerAppImagePatch {
            current_version: Some(version),
            creation_time: Some(created.0),
            ..Default::default()
        };
        block.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `loadOptionProperty` (`0x14001FC10`).
    ///
    /// At most [`MAX_OPTIONS`] entries are rewritten and the entries behind the count keep their previous contents,
    /// but `option_count` is the unclamped libam count (read a second time).
    fn load_option_property(&self, block: &mut ServerAppImageMut<'_>) {
        block.option_count_mut().set(0).expect("scalar fields accept every value");
        if self.port.acquire_app_image().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let rewritten = self.port.option_count().min(MAX_OPTIONS as u32);
        let mut infos = block.option_infos_mut();
        for index in 0..rewritten {
            let (id, created) = convert_option(self.port.option_property(index).as_ref());
            let mut entry = infos.get_mut(index as usize).expect("below the array capacity");
            publish_option(&mut entry, &id, created);
        }
        block.option_count_mut().set(self.port.option_count()).expect("scalar fields accept every value");
    }

    /// `loadOptionRootPath` (`0x14001FDA0`): `wcsncpy_s(path, 0x100, src, 0xFF)` copies at most 255 units and the
    /// first unit is cleared before it, so a shorter root leaves the tail of the previous one behind the terminator.
    fn load_option_root_path(&self, block: &mut ServerAppImageMut<'_>) {
        let root = if self.port.acquire_app_image().is_ok() {
            self.port.option_root()
        } else {
            tracing::warn!("libam is not ready");
            None
        };
        let mut path = block.option_root_path_mut();
        let fitted = fit_wide(root.as_ref().map(|root| root.as_slice()).unwrap_or_default(), path.capacity());
        path.set(&fitted).expect("fitted to the field");
    }
}

impl Controller for AppImageController {
    fn id(&self) -> ControllerId {
        ControllerId::AppImage
    }

    /// vf5 `0x14001FAC0` (ready rising edge): everything is loaded once.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.app_image_mut();
        self.load_app_property(&mut block);
        self.load_option_property(&mut block);
        self.load_option_root_path(&mut block);
    }

    /// APM listener (lambda `5ddb45cb`, `0x1400202A0`): the state value is ignored; the root path is not reloaded.
    fn on_app_state(&mut self, _state: u32, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.app_image_mut();
        self.load_app_property(&mut block);
        self.load_option_property(&mut block);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use widestring::U16CStr;

    use libam::result::{AmError, AmResult};
    use libam::time::Tm;

    use super::*;
    use crate::controllers::controller::testing::{Harness, civil};

    #[derive(Default)]
    struct Libam {
        down: bool,
        app: Option<AppImageProperty>,
        options: Vec<AppImageProperty>,
        root: Option<[u16; 260]>,
        asked: Vec<u32>,
    }

    type Shared = Rc<RefCell<Libam>>;

    struct FakePort(Shared);

    impl AppImagePort for FakePort {
        fn acquire_app_image(&self) -> AmResult {
            if self.0.borrow().down { Err(AmError::code(2)) } else { Ok(()) }
        }

        fn current_app(&self) -> Option<AppImageProperty> {
            self.0.borrow().app
        }

        fn option_count(&self) -> u32 {
            self.0.borrow().options.len() as u32
        }

        fn option_property(&self, index: u32) -> Option<AppImageProperty> {
            let mut libam = self.0.borrow_mut();
            libam.asked.push(index);
            libam.options.get(index as usize).copied()
        }

        fn option_root(&self) -> Option<[u16; 260]> {
            self.0.borrow().root
        }
    }

    const fn wide<const N: usize>(text: &str) -> [u16; N] {
        let bytes = text.as_bytes();
        let mut units = [0u16; N];
        let mut index = 0;
        while index < bytes.len() && index < N {
            units[index] = bytes[index] as u16;
            index += 1;
        }
        units
    }

    static OPTION_ROOT: [u16; 260] = wide("C:\\mount\\Option\\");
    static SHORT_ROOT: [u16; 260] = wide("D:\\");
    static LONG_ROOT: [u16; 260] = [b'a' as u16; 260];

    fn libam() -> Shared {
        Rc::new(RefCell::new(Libam::default()))
    }

    fn controller(libam: &Shared) -> AppImageController {
        AppImageController::new(Box::new(FakePort(Rc::clone(libam))))
    }

    fn tm(year: i32, month: i32, day: i32, hour: i32, min: i32, sec: i32) -> Tm {
        Tm { sec, min, hour, mday: day, mon: month - 1, year: year - 1900, ..Tm::default() }
    }

    fn some_tm() -> Tm {
        tm(2023, 5, 6, 7, 8, 9)
    }

    /// `amdaemon::Version::make` (`0x140062E60`).
    const fn version(major: u16, minor: u8, patch: u8) -> u32 {
        (major as u32) << 16 | (minor as u32) << 8 | patch as u32
    }

    fn app(version: u32, creation: Tm) -> AppImageProperty {
        AppImageProperty { kind: PropertyKind::App, initialized: true, version, option_id: [0; 5], creation }
    }

    fn option(id: &str, creation: Tm) -> AppImageProperty {
        let mut option_id = [0u16; 5];
        for (slot, unit) in option_id.iter_mut().zip(id.encode_utf16()) {
            *slot = unit;
        }
        AppImageProperty { kind: PropertyKind::Option, initialized: false, version: 0, option_id, creation }
    }

    fn text(units: &U16CStr) -> String {
        String::from_utf16_lossy(units.as_slice())
    }

    fn name(h: &Harness, index: usize) -> String {
        text(h.server.app_image().option_infos().get(index).expect("entry below 200").name())
    }

    /// `ServerData+0x19110`. Stale units behind a terminator are not visible through the checked strings, so
    /// these tests read the published bytes at the recovered offsets.
    const BLOCK: usize = 0x19110;
    /// `option_infos[0].name`: the array starts at `+0x18` and the name at `+0x08` of an entry.
    const FIRST_NAME: usize = 0x18 + 0x08;
    /// `option_root_path`.
    const ROOT_PATH: usize = 0x12D8;

    /// `N` published units at `offset` of the block.
    fn units<const N: usize>(h: &Harness, offset: usize) -> [u16; N] {
        let bytes = &h.server.bytes()[BLOCK + offset..][..2 * N];
        core::array::from_fn(|index| u16::from_le_bytes([bytes[2 * index], bytes[2 * index + 1]]))
    }

    #[test]
    fn rising_edge_publishes_the_app_the_options_and_the_option_root() {
        let libam = libam();
        {
            let mut libam = libam.borrow_mut();
            libam.app = Some(app(version(1, 2, 3), some_tm()));
            libam.options = vec![option("ABCD", tm(2022, 1, 2, 3, 4, 5)), option("WX9Z", tm(2021, 12, 31, 23, 59, 58))];
            libam.root = Some(OPTION_ROOT);
        }
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());

        let data = h.server.app_image();
        assert_eq!(data.current_version(), version(1, 2, 3));
        assert_eq!(data.creation_time(), civil(2023, 5, 6, 7, 8, 9).0);
        assert_eq!(data.option_count(), 2);
        let infos = data.option_infos();
        let created = |index: usize| infos.get(index).expect("entry below 200").creation_time();
        assert_eq!(created(0), civil(2022, 1, 2, 3, 4, 5).0);
        assert_eq!(created(1), civil(2021, 12, 31, 23, 59, 58).0);
        assert_eq!((name(&h, 0), name(&h, 1)), ("ABCD".to_owned(), "WX9Z".to_owned()));
        // The entry behind the count stays blank.
        assert_eq!((created(2), name(&h, 2)), (0, String::new()));
        assert_eq!(text(data.option_root_path()), "C:\\mount\\Option\\");
    }

    #[test]
    fn property_validity_requires_the_id_or_init_byte_and_a_positive_day() {
        let valid_app = app(version(1, 0, 0), some_tm());
        assert!(valid_app.is_valid());
        assert!(!AppImageProperty { initialized: false, ..valid_app }.is_valid());
        assert!(!AppImageProperty { creation: Tm { mday: 0, ..some_tm() }, ..valid_app }.is_valid());
        assert!(AppImageProperty { kind: PropertyKind::Os, ..valid_app }.is_valid());

        assert!(option("AB1D", some_tm()).is_valid());
        assert!(!option("ABC", some_tm()).is_valid());
        assert!(!option("AB-D", some_tm()).is_valid());
        // Five characters leave no terminator inside `wchar_t[5]`.
        assert!(!option("ABCDE", some_tm()).is_valid());
        // The init byte is irrelevant for options.
        assert!(AppImageProperty { initialized: true, ..option("ABCD", some_tm()) }.is_valid());
    }

    #[test]
    fn missing_or_invalid_app_resets_the_published_version_and_time_silently() {
        let libam = libam();
        libam.borrow_mut().app = Some(app(version(1, 2, 3), some_tm()));
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());
        assert_eq!(h.server.app_image().current_version(), version(1, 2, 3));

        libam.borrow_mut().app = None;
        controller.on_app_state(4, &mut h.ctx());
        assert_eq!((h.server.app_image().current_version(), h.server.app_image().creation_time()), (0, 0));

        libam.borrow_mut().app = Some(AppImageProperty { initialized: false, ..app(version(2, 0, 0), some_tm()) });
        controller.on_app_state(4, &mut h.ctx());
        assert_eq!(h.server.app_image().current_version(), 0);
    }

    #[test]
    fn bad_timestamps_and_wrong_property_kinds_publish_zero() {
        let libam = libam();
        let bad_time = Tm { mon: 12, ..some_tm() };
        {
            let mut libam = libam.borrow_mut();
            libam.app = Some(app(version(1, 0, 0), bad_time));
            let wrong_kind =
                AppImageProperty { kind: PropertyKind::App, initialized: true, ..option("EFGH", some_tm()) };
            libam.options = vec![option("ABCD", bad_time), wrong_kind];
        }
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.on_app_state(1, &mut h.ctx());

        assert_eq!(h.server.app_image().current_version(), 0);
        assert_eq!(h.server.app_image().creation_time(), 0);
        assert_eq!((h.server.app_image().option_count(), name(&h, 0), name(&h, 1)), (2, String::new(), String::new()));

        // An option image in the app slot also publishes zeros.
        libam.borrow_mut().app = Some(option("ABCD", some_tm()));
        controller.on_app_state(1, &mut h.ctx());
        assert_eq!((h.server.app_image().current_version(), h.server.app_image().creation_time()), (0, 0));
    }

    #[test]
    fn option_entries_keep_the_stale_name_tail_when_the_property_turns_invalid() {
        let libam = libam();
        libam.borrow_mut().options = vec![option("ABCD", some_tm())];
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());
        assert_eq!(name(&h, 0), "ABCD");

        libam.borrow_mut().options = vec![option("AB", some_tm())];
        controller.on_app_state(1, &mut h.ctx());
        // `wcsncpy_s(name, 5, L"", 4)` terminates the first unit and leaves the rest.
        assert_eq!(units::<5>(&h, FIRST_NAME), [0, u16::from(b'B'), u16::from(b'C'), u16::from(b'D'), 0]);
        assert_eq!(h.server.app_image().option_infos().get(0).expect("first entry").creation_time(), 0);
    }

    #[test]
    fn option_copy_is_clamped_to_200_but_the_count_is_not_and_old_entries_stay() {
        let libam = libam();
        let created = tm(2020, 1, 1, 0, 0, 0);
        libam.borrow_mut().options = (0..250).map(|i| option(&format!("A{i:03}"), created)).collect();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());

        assert_eq!(h.server.app_image().option_count(), 250);
        assert_eq!(name(&h, 199), "A199");
        let asked = libam.borrow().asked.clone();
        assert_eq!((asked.len(), asked.last().copied()), (200, Some(199)));

        // A shorter list rewrites only its own entries.
        libam.borrow_mut().options.truncate(120);
        controller.on_app_state(1, &mut h.ctx());
        assert_eq!(h.server.app_image().option_count(), 120);
        assert_eq!(
            (name(&h, 119), name(&h, 120), name(&h, 199)),
            ("A119".to_owned(), "A120".to_owned(), "A199".to_owned())
        );
    }

    #[test]
    fn missing_libam_leaves_only_the_reset_fields() {
        let libam = libam();
        {
            let mut libam = libam.borrow_mut();
            libam.app = Some(app(version(1, 2, 3), some_tm()));
            libam.options = vec![option("ABCD", some_tm())];
            libam.root = Some(OPTION_ROOT);
        }
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());

        libam.borrow_mut().down = true;
        controller.clear_server_data(&mut h.ctx());

        let data = h.server.app_image();
        assert_eq!((data.current_version(), data.creation_time(), data.option_count()), (0, 0, 0));
        // Entries and the tail of the root path are not touched.
        assert_eq!(name(&h, 0), "ABCD");
        assert_eq!(units::<2>(&h, ROOT_PATH), [0, u16::from(b':')]);
    }

    #[test]
    fn app_state_reloads_app_and_options_but_not_the_root_path() {
        let libam = libam();
        {
            let mut libam = libam.borrow_mut();
            libam.app = Some(app(version(1, 0, 0), some_tm()));
            libam.root = Some(OPTION_ROOT);
        }
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());

        {
            let mut libam = libam.borrow_mut();
            libam.app = Some(app(version(2, 0, 0), some_tm()));
            libam.options = vec![option("ZZZZ", some_tm())];
            libam.root = Some(SHORT_ROOT);
        }
        controller.on_app_state(0, &mut h.ctx());

        assert_eq!(h.server.app_image().current_version(), version(2, 0, 0));
        assert_eq!((h.server.app_image().option_count(), name(&h, 0)), (1, "ZZZZ".to_owned()));
        assert_eq!(text(h.server.app_image().option_root_path()), "C:\\mount\\Option\\");
    }

    #[test]
    fn root_path_is_cut_at_255_units_and_a_shorter_root_leaves_the_old_tail() {
        let libam = libam();
        libam.borrow_mut().root = Some(LONG_ROOT);
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());
        let path = units::<256>(&h, ROOT_PATH);
        assert_eq!((h.server.app_image().option_root_path().len(), path[254], path[255]), (255, u16::from(b'a'), 0));

        libam.borrow_mut().root = Some(SHORT_ROOT);
        controller.clear_server_data(&mut h.ctx());
        let path = units::<256>(&h, ROOT_PATH);
        assert_eq!(text(h.server.app_image().option_root_path()), "D:\\");
        assert_eq!((path[3], path[4]), (0, u16::from(b'a')));
    }
}
