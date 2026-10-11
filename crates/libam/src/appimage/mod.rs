//! The application image catalogue (`am::appimage::Manager`, `spec/libam/storage.md` part 4): a read-only,
//! load-once snapshot of the installer index file (ICF) on the AMFS volume.
//!
//! # Cross-module methods
//!
//! * [`AppImage::initialize`] (`0x140244AF0`, 4.4.2): reads the ICF (`amAppImageInit`, [`icf::init`]); a failure
//!   leaves the manager without its context and the loader latches 4105.
//! * [`AppImage::update`] (`0x140244C30`, M.3): the loader runs in states 0 and 1 only, so exactly once after
//!   `initialize` and again after a re-arm.
//! * [`AppImage::is_ready`] (`0x140244C70`): `allReady` term, state 2 or 3.
//! * [`AppImage::restart`] (`0x140244C50`): re-arms the loader (states 2 and 3 only); the restart sweep calls it
//!   when it is `forced`. The ICF is not read again.
//! * [`AppImage::current_app`] (`0x140244D60`): the last (highest-version) application, `None` outside state 2.
//!   `changeApp` and the ABaaS update read it (4.7).

pub mod icf;
mod port;

#[cfg(test)]
mod tests;

use libam_api::apm::version_parts;
use libam_api::app_image::{AppImageProperty, PropertyKind};
use libam_api::time::Tm;

use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use icf::{Entries, Entry, EntryKind};

pub use port::AppImageHandle;

/// Error code of a failed load (4.6).
const ERROR_LOAD: u16 = 4105;
/// `C:\mount\Option` + the trailing `\` the loader appends (4.10.9).
const OPTION_ROOT: &str = r"C:\mount\Option\";
const APP_CAPACITY: usize = 10;
const OS_CAPACITY: usize = 1;
const OPTION_CAPACITY: usize = 100;

/// `am::appimage::Manager` (`ctx+55192`).
#[derive(Debug, Default)]
pub struct AppImage {
    life: Lifecycle,
    /// The shared catalogue context exists (`handleHeld`).
    handle_held: bool,
    /// The static arrays of the C-level module (`amAppImageInit`).
    entries: Entries,
    apps: Vec<AppImageProperty>,
    os: Vec<AppImageProperty>,
    options: Vec<AppImageProperty>,
    option_root: Option<[u16; 260]>,
}

/// A property in its reset state (4.2.1): App type, not valid, id `----`.
fn reset_property() -> AppImageProperty {
    AppImageProperty {
        kind: PropertyKind::App,
        initialized: false,
        version: 0,
        option_id: [u16::from(b'-'), u16::from(b'-'), u16::from(b'-'), u16::from(b'-'), 0],
        creation: Tm::default(),
    }
}

/// The App / OS setters (`0x14021FAE0`, `0x14021FCD0`): version limits 9999 / 99 / 99.
fn set_version(property: &mut AppImageProperty, kind: PropertyKind, raw: u32) {
    let (major, minor, patch) = version_parts(raw);
    if major <= 9999 && minor <= 99 && patch <= 99 {
        property.initialized = true;
        property.version = raw;
        property.kind = kind;
    }
}

/// The Option setter (`0x14021FC00`): the id must decode (code page 932, strict) to exactly four characters.
fn set_option(property: &mut AppImageProperty, id: [u8; 4]) {
    let Some(text) = encoding_rs::SHIFT_JIS.decode_without_bom_handling_and_without_replacement(&id) else {
        return;
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() == 4 {
        property.option_id = [units[0], units[1], units[2], units[3], 0];
        property.kind = PropertyKind::Option;
    }
}

/// Property from an entry (`0x140245450`).
fn build_property(entry: &Entry) -> AppImageProperty {
    let mut property = reset_property();
    match entry.kind {
        EntryKind::App => set_version(&mut property, PropertyKind::App, entry.version),
        EntryKind::Os => set_version(&mut property, PropertyKind::Os, entry.version),
        EntryKind::Option => set_option(&mut property, entry.option_id),
    }
    property.creation = Tm {
        sec: i32::from(entry.second),
        min: i32::from(entry.minute),
        hour: i32::from(entry.hour),
        mday: i32::from(entry.day),
        mon: if entry.month != 0 { i32::from(entry.month) - 1 } else { 0 },
        year: if entry.year < 1900 { 0 } else { i32::from(entry.year) - 1900 },
        wday: 0,
        yday: 0,
        isdst: 0,
    };
    property
}

impl AppImage {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Manager::initialize` (`0x140244AF0`): creates the catalogue context (reads the ICF); the state is 1 in
    /// every case.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.life.begin();
        if self.handle_held {
            self.life.error.set(2, 0);
            return;
        }
        match icf::init(cx.backend) {
            Ok(entries) => {
                self.entries = entries;
                self.handle_held = true;
            }
            Err(code) => {
                tracing::warn!(code, "app image: the catalogue could not be opened");
            }
        }
    }

    /// `Manager::execute` (`0x140244C30`): the loader `0x140244DC0` while the state is 0 or 1.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if self.life.state.0 > 1 {
            return;
        }
        if !self.handle_held {
            self.life.state = StateWord::FAILED;
            self.life.error.set(ERROR_LOAD, 0);
            return;
        }
        let mut root = [0u16; 260];
        for (slot, unit) in root.iter_mut().zip(OPTION_ROOT.encode_utf16()) {
            *slot = unit;
        }
        self.option_root = Some(root);
        self.apps = self.entries.apps.iter().take(APP_CAPACITY).map(build_property).collect();
        self.os = self.entries.os.iter().take(OS_CAPACITY).map(build_property).collect();
        self.options = self.entries.options.iter().take(OPTION_CAPACITY).map(build_property).collect();
        self.life.state = StateWord::READY;
    }

    /// Shutdown (`0x140244BF0`): releases the context (the static arrays go with it), state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if self.life.state != StateWord::UNINITIALIZED {
            *self = Self::default();
        }
    }

    /// Re-arm (`0x140244C50`): only in states 2 and 3; the properties are dropped and rebuilt by the next loader
    /// run from the unchanged static arrays, state 1.
    pub fn restart(&mut self) {
        if self.life.state.is_done() {
            self.life = Lifecycle::default();
            self.life.begin();
            self.apps.clear();
            self.os.clear();
            self.options.clear();
            self.option_root = None;
        }
    }

    /// `allReady` term (`0x140244C70`): state 2 or 3.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`0x140244C90`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    fn loaded(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// `currentApp` (`0x140244D60`): the last application of the sorted list while loaded.
    pub fn current_app(&self) -> Option<AppImageProperty> {
        if self.loaded() { self.apps.last().copied() } else { None }
    }

    /// `0x140244CA0` for the App type (`appCount`).
    pub fn app_count(&self) -> u32 {
        if self.loaded() { u32::try_from(self.apps.len()).unwrap_or(u32::MAX) } else { 0 }
    }

    /// `0x140244CA0` for the Option type.
    pub fn option_count(&self) -> u32 {
        if self.loaded() { u32::try_from(self.options.len()).unwrap_or(u32::MAX) } else { 0 }
    }

    /// Element `index` of the Option array (`0x140244CE0`).
    pub fn option_at(&self, index: u32) -> Option<AppImageProperty> {
        if self.loaded() { self.options.get(index as usize).copied() } else { None }
    }

    /// Element `index` of the OS array (type 2 of `optionProperty`).
    pub fn os_at(&self, index: u32) -> Option<AppImageProperty> {
        if self.loaded() { self.os.get(index as usize).copied() } else { None }
    }

    /// Element `index` of the App array.
    pub fn app_at(&self, index: u32) -> Option<AppImageProperty> {
        if self.loaded() { self.apps.get(index as usize).copied() } else { None }
    }

    /// `optionRoot` (`0x140217100`): `Some` in state 2 only.
    pub fn option_root(&self) -> Option<[u16; 260]> {
        if self.loaded() { self.option_root } else { None }
    }
}
