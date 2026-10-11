//! `amdaemon::server::allnet::WiFiController` (vtable `0x1405E4538`, `App+0xD80`, `0x28` bytes).
//!
//! Only `updateServerData` (`0x140051B60`) is overridden: every tick while ready it republishes libam's
//! Wi-Fi unit list into `ServerData+0x47FC`. There is no clear step, executor, client block or listener
//! (spec `controllers/allnet.md` §4; evidence ids `allnet.wifi.*`).

use crate::schema::allnet::{MAX_WIFI_UNITS, ServerWiFiPatch, ServerWiFiUnitMut, ServerWiFiUnitPatch};
use crate::schema::text::fit_wide;
use libam::allnet::wifi::{WiFiPort, WifiUnit};

use crate::controllers::controller::{Controller, ControllerId, Ctx};

/// Master serial published when libam knows none (`L"N/A"`).
const NOT_AVAILABLE: [u16; 3] = [b'N' as u16, b'/' as u16, b'A' as u16];

/// `WiFiController`.
pub struct WiFiController {
    port: Box<dyn WiFiPort>,
}

impl std::fmt::Debug for WiFiController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WiFiController").finish_non_exhaustive()
    }
}

impl WiFiController {
    /// Constructs the controller (ctor `0x140051B10` = base ctor + vptr).
    pub fn new(port: Box<dyn WiFiPort>) -> Self {
        Self { port }
    }
}

/// Fills one wire slot from a populated libam unit.
///
/// **Faithful quirk:** both flag bytes receive libam's `serverAlive` flag (`WifiUnit+0x46`, the getter
/// `0x140227C70` is called twice); libam's `authGood` flag is never read.
fn publish_unit(slot: &mut ServerWiFiUnitMut<'_>, unit: &WifiUnit) {
    let scalars = ServerWiFiUnitPatch {
        auth_good: Some(unit.server_alive),
        server_alive: Some(unit.server_alive),
        access_count: Some(unit.access_count as u16),
        ..ServerWiFiUnitPatch::default()
    };
    slot.copy_from(&scalars).expect("scalar fields accept every value");

    let mut field = slot.serial_mut();
    let fitted = fit_wide(&unit.serial, field.capacity());
    field.set(&fitted).expect("fitted to the field");
    let mut field = slot.firm_version_mut();
    let fitted = fit_wide(&unit.firm, field.capacity());
    field.set(&fitted).expect("fitted to the field");
    let mut field = slot.master_serial_mut();
    let master = if unit.master[0] == 0 { &NOT_AVAILABLE[..] } else { &unit.master[..] };
    let fitted = fit_wide(master, field.capacity());
    field.set(&fitted).expect("fitted to the field");
}

impl Controller for WiFiController {
    fn id(&self) -> ControllerId {
        ControllerId::AllnetWiFi
    }

    /// vf6 `updateServerData` (`0x140051B60`).
    ///
    /// The unit area is zeroed on every update, then the first `min(10, count)` units are filled.
    /// `unitCount` is not reduced when a unit is skipped (getter failure or unpopulated unit): that slot
    /// simply stays zero.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.acquire_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut block = ctx.server.allnet_wifi_mut();
        let available = self.port.available();
        let count = self.port.unit_count().min(MAX_WIFI_UNITS as u64);
        let header = ServerWiFiPatch {
            available: Some(available),
            unit_count: Some(count as u32),
            ..ServerWiFiPatch::default()
        };
        block.copy_from(&header).expect("scalar fields accept every value");
        let mut units = block.units_mut();
        // `memset(sd+8, 0, 0x3D4)`: the whole unit area, every slot.
        for index in 0..MAX_WIFI_UNITS {
            units.get_mut(index).expect("declared element").clear();
        }
        for index in 0..count {
            let Ok(unit) = self.port.unit(index) else {
                tracing::warn!("libam version does not match");
                continue;
            };
            if unit.serial[0] == 0 {
                tracing::warn!("libam version does not match");
                continue;
            }
            let mut slot = units.get_mut(index as usize).expect("the count is clamped to the unit capacity");
            publish_unit(&mut slot, &unit);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use widestring::U16CStr;

    use libam::result::{AmError, AmResult};

    use super::*;
    use crate::controllers::controller::testing::Harness;

    /// `ServerData` offset of the WiFi block.
    const BLOCK: usize = 0x47FC;
    /// Size of the WiFi block.
    const BLOCK_SIZE: usize = 0x3DC;
    /// Block offset of the first unit and the size of one.
    const UNITS: usize = 8;
    const UNIT_SIZE: usize = 98;

    #[derive(Debug, Default)]
    struct FakeWifi {
        unavailable: bool,
        available: bool,
        reported_count: u64,
        units: Vec<WifiUnit>,
        requested: Rc<RefCell<Vec<u64>>>,
    }

    impl WiFiPort for FakeWifi {
        fn acquire_handle(&self) -> AmResult<()> {
            if self.unavailable { Err(AmError::code(2)) } else { Ok(()) }
        }
        fn available(&self) -> bool {
            self.available
        }
        fn unit_count(&self) -> u64 {
            self.reported_count
        }
        fn unit(&self, index: u64) -> AmResult<WifiUnit> {
            self.requested.borrow_mut().push(index);
            self.units.get(index as usize).copied().ok_or(AmError::code(1))
        }
    }

    fn wide<const N: usize>(text: &str) -> [u16; N] {
        let mut wide = [0; N];
        for (dst, unit) in wide.iter_mut().zip(text.encode_utf16()) {
            *dst = unit;
        }
        wide
    }

    fn unit(serial: &str) -> WifiUnit {
        WifiUnit {
            serial: wide(serial),
            auth_good: false,
            firm: wide("1.2.3"),
            server_alive: true,
            access_count: 7,
            master: wide("MASTER"),
        }
    }

    fn text(units: &U16CStr) -> String {
        String::from_utf16_lossy(units.as_slice())
    }

    fn fake(units: Vec<WifiUnit>) -> FakeWifi {
        FakeWifi { available: true, reported_count: units.len() as u64, units, ..FakeWifi::default() }
    }

    fn tick(harness: &mut Harness, port: FakeWifi) {
        WiFiController::new(Box::new(port)).update_server_data(&mut harness.ctx());
    }

    /// The raw bytes of the WiFi block, to see padding and the units behind a terminator.
    fn raw(harness: &Harness) -> &[u8] {
        &harness.server.bytes()[BLOCK..BLOCK + BLOCK_SIZE]
    }

    /// The raw bytes of unit `index`.
    fn unit_bytes(harness: &Harness, index: usize) -> &[u8] {
        &raw(harness)[UNITS + index * UNIT_SIZE..UNITS + (index + 1) * UNIT_SIZE]
    }

    #[test]
    fn unavailable_libam_writes_nothing() {
        let mut h = Harness::running();
        tick(&mut h, fake(vec![unit("A"), unit("B")]));
        h.server.span_mut(BLOCK + 1, 3).fill(0x33);
        let before = raw(&h).to_vec();
        let changed = FakeWifi { available: false, unavailable: true, ..fake(vec![unit("C")]) };
        tick(&mut h, changed);
        assert_eq!(raw(&h), before.as_slice());
    }

    #[test]
    fn publishes_availability_count_and_unit_fields() {
        let mut h = Harness::running();
        let mut first = unit("SER-0001");
        first.access_count = 0x1_2345;
        first.master = [0; 13];
        tick(&mut h, fake(vec![first, unit("SER-0002")]));
        let sd = h.server.allnet_wifi();
        assert!(sd.available());
        assert_eq!(sd.unit_count(), 2);
        let units = sd.units();
        let u0 = units.get(0).expect("unit 0");
        assert_eq!(text(u0.serial()), "SER-0001");
        assert_eq!(text(u0.firm_version()), "1.2.3");
        assert_eq!(text(u0.master_serial()), "N/A", "an empty master serial is published as N/A");
        assert_eq!(u0.access_count(), 0x2345, "only the low 16 bits of the access count are kept");
        assert_eq!(text(units.get(1).expect("unit 1").master_serial()), "MASTER");
        assert!(unit_bytes(&h, 2).iter().all(|&byte| byte == 0));
    }

    #[test]
    fn both_flag_bytes_carry_the_server_alive_flag() {
        for (auth_good, server_alive) in [(true, false), (false, true), (true, true), (false, false)] {
            let mut h = Harness::running();
            let mut u = unit("S");
            u.auth_good = auth_good;
            u.server_alive = server_alive;
            tick(&mut h, fake(vec![u]));
            let units = h.server.allnet_wifi().units();
            let slot = units.get(0).expect("unit 0");
            assert_eq!((slot.auth_good(), slot.server_alive()), (server_alive, server_alive));
        }
    }

    #[test]
    fn unit_count_is_clamped_to_ten_and_only_ten_units_are_read() {
        let mut h = Harness::running();
        let units: Vec<WifiUnit> = (0..12).map(|i| unit(&format!("S{i}"))).collect();
        let requested = Rc::new(RefCell::new(Vec::new()));
        let port = FakeWifi { reported_count: 25, requested: Rc::clone(&requested), ..fake(units) };
        tick(&mut h, port);
        let sd = h.server.allnet_wifi();
        assert_eq!(sd.unit_count(), 10);
        assert_eq!(text(sd.units().get(9).expect("unit 9").serial()), "S9");
        assert_eq!(*requested.borrow(), (0..10).collect::<Vec<u64>>());
    }

    #[test]
    fn unpopulated_unit_stays_zero_but_is_still_counted() {
        let mut h = Harness::running();
        let mut hollow = unit("");
        hollow.server_alive = true;
        tick(&mut h, fake(vec![unit("A"), hollow, unit("C")]));
        let sd = h.server.allnet_wifi();
        assert_eq!(sd.unit_count(), 3);
        let units = sd.units();
        assert_eq!(text(units.get(0).expect("unit 0").serial()), "A");
        assert!(unit_bytes(&h, 1).iter().all(|&byte| byte == 0));
        assert_eq!(text(units.get(2).expect("unit 2").serial()), "C");
    }

    #[test]
    fn the_unit_area_is_zeroed_before_every_update() {
        let mut h = Harness::running();
        tick(&mut h, fake(vec![unit("LONG-SERIAL"), unit("B"), unit("C")]));
        tick(&mut h, fake(vec![unit("X")]));
        assert_eq!(h.server.allnet_wifi().unit_count(), 1);
        let serial: Vec<u16> = unit_bytes(&h, 0)[4..4 + 2 * 4]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(serial, [u16::from(b'X'), 0, 0, 0], "no stale tail from the longer serial");
        assert!(unit_bytes(&h, 1).iter().all(|&byte| byte == 0));
        assert!(unit_bytes(&h, 2).iter().all(|&byte| byte == 0));
    }
}
