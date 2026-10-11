//! `amdaemon::server::allnet::AuthController` (vtable `0x1405E3DC8`, `App+0xCD8`, `0x28` bytes).
//!
//! The controller only overrides `updateServerData` (`0x1400513F0`): it mirrors libam's ALL.Net
//! authentication state into `ServerData+0x4BE0` every tick while ready. It has no clear step, no
//! executor, no client block and no listeners, so the block stays all-zero until libam first reports a
//! good authentication (spec `controllers/allnet.md` §2; evidence ids `allnet.auth.*`).

use widestring::U16CStr;

use crate::schema::allnet::{AuthLineType, ServerAuthMut, ServerAuthPatch};
use crate::schema::text::{fit_narrow, fit_wide};
use libam::allnet::{AuthLocation, AuthPort};

use crate::controllers::controller::{Controller, ControllerId, Ctx};

/// `AuthController`.
pub struct AuthController {
    port: Box<dyn AuthPort>,
}

impl std::fmt::Debug for AuthController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthController").finish_non_exhaustive()
    }
}

impl AuthController {
    /// Constructs the controller (ctor `0x1400513A0` = base ctor + vptr).
    pub fn new(port: Box<dyn AuthPort>) -> Self {
        Self { port }
    }
}

/// `lineType` from libam's line mask (`2`→1, `4`→2, `8`→3, anything else→0).
fn line_type(mask: u32) -> AuthLineType {
    match mask {
        2 => AuthLineType::Broadband,
        4 => AuthLineType::Mobile,
        8 => AuthLineType::Xdsl,
        _ => AuthLineType::NoLine,
    }
}

/// Publishes one libam string getter result into a field of `capacity` units (`wcsncpy_s` semantics).
/// Returns false when the getter failed.
fn publish<const N: usize>(src: Option<&[u16; N]>, capacity: usize, store: impl FnOnce(&U16CStr)) -> bool {
    match src {
        Some(units) => {
            store(&fit_wide(units, capacity));
            true
        }
        None => false,
    }
}

/// The location branch of `updateServerData`, in the binary's write order.
fn publish_location(block: &mut ServerAuthMut<'_>, location: &AuthLocation) {
    let scalars = ServerAuthPatch {
        location_id: Some(location.id),
        develop: Some(location.develop),
        region_code: Some(location.region_code),
        ..ServerAuthPatch::default()
    };
    block.copy_from(&scalars).expect("scalar fields accept every value");

    let mut field = block.location_name_mut();
    if !publish(location.name.as_ref(), field.capacity(), |text| {
        field.set(text).expect("fitted to the field");
    }) {
        tracing::warn!("libam version does not match");
    }
    let mut nicknames = block.location_nicknames_mut();
    for (index, src) in location.nicknames.iter().enumerate() {
        let mut field = nicknames.get_mut(index).expect("the nickname slot exists");
        if !publish(src.as_ref(), field.capacity(), |text| {
            field.set(text).expect("fitted to the field");
        }) {
            tracing::warn!("libam version does not match");
        }
    }
    let mut region_names = block.region_names_mut();
    for (index, src) in location.region_names.iter().enumerate() {
        let mut field = region_names.get_mut(index).expect("the region name slot exists");
        if !publish(src.as_ref(), field.capacity(), |text| {
            field.set(text).expect("fitted to the field");
        }) {
            tracing::warn!("libam version does not match");
        }
    }
    let mut field = block.country_code_mut();
    if !publish(location.country.as_ref(), field.capacity(), |text| {
        field.set(text).expect("fitted to the field");
    }) {
        tracing::warn!("libam version does not match");
    }
    let mut field = block.game_server_uri_mut();
    if !publish(location.uri.as_ref(), field.capacity(), |text| {
        field.set(text).expect("fitted to the field");
    }) {
        tracing::warn!("libam version does not match");
    }
    let mut field = block.game_server_host_mut();
    if !publish(location.host.as_ref(), field.capacity(), |text| {
        field.set(text).expect("fitted to the field");
    }) {
        tracing::warn!("libam version does not match");
    }
}

impl Controller for AuthController {
    fn id(&self) -> ControllerId {
        ControllerId::AllnetAuth
    }

    /// vf6 `updateServerData` (`0x1400513F0`).
    ///
    /// `good` is written on every update; while it is false the rest of the block keeps its previous
    /// snapshot. Without a location none of the location, region, country, URI and host fields change.
    /// Strings are published with `wcsncpy_s`/`strcpy_s` semantics: silent truncation and stale bytes
    /// after the terminator.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.acquire_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut block = ctx.server.allnet_auth_mut();
        let good = self.port.is_good();
        block.good_mut().set(good).expect("bool accepts true and false");
        if !good {
            return;
        }
        block.line_type_mut().set(line_type(self.port.line_mask())).expect("closed enum value");
        if let Some(location) = self.port.location() {
            publish_location(&mut block, &location);
        }
        if let Some(time) = self.port.time() {
            match time.to_datetime() {
                Some(last_auth_time) => {
                    block.last_auth_time_mut().set(last_auth_time.0).expect("scalar fields accept every value");
                }
                None => tracing::warn!("libam version does not match"),
            }
        }
        // `strcpy_s(sd+0x4C4, 0x600, text)`: libam's setter rejects texts of `0x600` or more characters, and
        // the field capacity cuts at 1535 characters in any case.
        let text = self.port.text();
        let mut field = block.auth_text_mut();
        let fitted = fit_narrow(text.as_ref().map(|text| text.as_slice()).unwrap_or_default(), field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }
}

#[cfg(test)]
mod tests {
    use libam::allnet::TimeSlot;
    use libam::result::{AmError, AmResult};
    use libam::sequence::ProcessState;
    use libam::time::{DateTime, Tm};

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, buffer, civil};

    /// `ServerData` offset of the Auth block.
    const BLOCK: usize = 0x4BE0;
    /// Size of the Auth block.
    const BLOCK_SIZE: usize = 0xAD0;
    /// Block offset of `location_name`.
    const LOCATION_NAME: usize = 0x210;
    /// Block offset of `auth_text`.
    const AUTH_TEXT: usize = 0x4C4;

    /// What the fake libam reports during one tick.
    #[derive(Debug, Default)]
    struct FakeAuth {
        unavailable: bool,
        good: bool,
        line_mask: u32,
        location: Option<AuthLocation>,
        time: Option<TimeSlot>,
        text: Option<Vec<u8>>,
    }

    impl AuthPort for FakeAuth {
        fn acquire_handle(&self) -> AmResult<()> {
            if self.unavailable { Err(AmError::code(2)) } else { Ok(()) }
        }
        fn is_good(&self) -> bool {
            self.good
        }
        fn line_mask(&self) -> u32 {
            self.line_mask
        }
        fn location(&self) -> Option<AuthLocation> {
            self.location
        }
        fn time(&self) -> Option<TimeSlot> {
            self.time
        }
        fn text(&self) -> Option<[u8; 1536]> {
            self.text.as_deref().map(buffer)
        }
    }

    fn wide<const N: usize>(text: &str) -> [u16; N] {
        let mut wide = [0; N];
        for (dst, unit) in wide.iter_mut().zip(text.encode_utf16()) {
            *dst = unit;
        }
        wide
    }

    fn text(units: &U16CStr) -> String {
        String::from_utf16_lossy(units.as_slice())
    }

    fn location() -> AuthLocation {
        AuthLocation {
            id: 0x1234,
            develop: true,
            name: Some(wide("Shop")),
            nicknames: [Some(wide("Nick0")), Some(wide("Nick1"))],
            region_code: -5,
            region_names: [Some(wide("R0")), Some(wide("R1")), Some(wide("R2")), Some(wide("R3"))],
            country: Some(wide("JPN")),
            uri: Some(wide("http://uri.example/")),
            host: Some(wide("host.example")),
        }
    }

    fn good() -> FakeAuth {
        FakeAuth { good: true, line_mask: 4, location: Some(location()), ..FakeAuth::default() }
    }

    fn tm(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Tm {
        Tm { sec, min, hour, mday, mon, year, ..Tm::default() }
    }

    /// One `vf6` call with a fresh controller (the controller keeps no state).
    fn tick(harness: &mut Harness, port: FakeAuth) {
        AuthController::new(Box::new(port)).update_server_data(&mut harness.ctx());
    }

    /// The raw bytes of the Auth block, to see padding and the units behind a terminator.
    fn raw(harness: &Harness) -> &[u8] {
        &harness.server.bytes()[BLOCK..BLOCK + BLOCK_SIZE]
    }

    /// `count` raw UTF-16 units at block offset `offset`.
    fn raw_units(harness: &Harness, offset: usize, count: usize) -> Vec<u16> {
        raw(harness)[offset..offset + 2 * count]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    #[test]
    fn unavailable_libam_writes_nothing() {
        let mut h = Harness::running();
        tick(&mut h, FakeAuth { text: Some(b"token".to_vec()), ..good() });
        h.server.span_mut(BLOCK + 2, 2).fill(0x5A);
        let before = raw(&h).to_vec();
        let changed = FakeAuth {
            unavailable: true,
            line_mask: 8,
            location: Some(AuthLocation { id: 0x4321, ..location() }),
            time: Some(TimeSlot { valid: true, tm: tm(124, 4, 20, 1, 2, 3) }),
            text: Some(b"other".to_vec()),
            ..good()
        };
        tick(&mut h, changed);
        assert_eq!(raw(&h), before.as_slice());
    }

    #[test]
    fn a_failed_auth_result_updates_only_good_and_keeps_the_previous_snapshot() {
        let mut h = Harness::running();
        tick(&mut h, good());
        let snapshot = raw(&h).to_vec();
        assert!(h.server.allnet_auth().good());

        tick(&mut h, FakeAuth { good: false, line_mask: 8, text: Some(b"new".to_vec()), ..FakeAuth::default() });
        let after = raw(&h);
        assert_eq!(after[0], 0, "good is rewritten");
        assert_eq!(&after[1..], &snapshot[1..], "everything else keeps the old snapshot");
    }

    #[test]
    fn publishes_location_region_endpoints_time_and_text() {
        let mut h = Harness::running();
        let fake = FakeAuth {
            time: Some(TimeSlot { valid: true, tm: tm(124, 4, 20, 12, 34, 56) }),
            text: Some(b"dG9rZW4=".to_vec()),
            ..good()
        };
        tick(&mut h, fake);
        let sd = h.server.allnet_auth();
        assert!(sd.good());
        assert!(sd.develop());
        assert_eq!(sd.line_type(), AuthLineType::Mobile);
        assert_eq!(sd.location_id(), 0x1234);
        assert_eq!(text(sd.location_name()), "Shop");
        let nicknames: Vec<String> = sd.location_nicknames().iter().map(text).collect();
        assert_eq!(nicknames, ["Nick0", "Nick1"]);
        assert_eq!(sd.region_code(), -5);
        let regions: Vec<String> = sd.region_names().iter().map(text).collect();
        assert_eq!(regions, ["R0", "R1", "R2", "R3"]);
        assert_eq!(text(sd.country_code()), "JPN");
        assert_eq!(text(sd.game_server_uri()), "http://uri.example/");
        assert_eq!(text(sd.game_server_host()), "host.example");
        assert_eq!(DateTime(sd.last_auth_time()), civil(2024, 5, 20, 12, 34, 56));
        assert_eq!(sd.auth_text().to_bytes(), b"dG9rZW4=");
    }

    #[test]
    fn without_a_location_the_location_fields_keep_their_previous_values() {
        let mut h = Harness::running();
        tick(&mut h, good());
        let fake = FakeAuth {
            line_mask: 8,
            location: None,
            time: Some(TimeSlot { valid: true, tm: tm(125, 0, 1, 0, 0, 0) }),
            text: Some(b"abc".to_vec()),
            ..FakeAuth::default()
        };
        tick(&mut h, FakeAuth { good: true, ..fake });
        let sd = h.server.allnet_auth();
        assert_eq!(sd.location_id(), 0x1234);
        assert_eq!(text(sd.location_name()), "Shop");
        assert_eq!(text(sd.game_server_host()), "host.example");
        assert_eq!(sd.line_type(), AuthLineType::Xdsl);
        assert_eq!(DateTime(sd.last_auth_time()), civil(2025, 1, 1, 0, 0, 0));
        assert_eq!(sd.auth_text().to_bytes(), b"abc");
    }

    #[test]
    fn strings_longer_than_the_wire_capacity_are_truncated_silently() {
        let mut h = Harness::running();
        let mut long = location();
        long.name = Some([u16::from(b'N'); 61]);
        long.country = Some([u16::from(b'C'); 4]);
        long.uri = Some([u16::from(b'U'); 129]);
        tick(&mut h, FakeAuth { location: Some(long), ..good() });
        let sd = h.server.allnet_auth();
        assert_eq!(sd.location_name().len(), 60);
        assert_eq!(raw_units(&h, LOCATION_NAME + 2 * 60, 1), [0]);
        assert_eq!(sd.country_code().len(), 3);
        assert_eq!(sd.game_server_uri().len(), 128);
    }

    #[test]
    fn a_shorter_string_leaves_stale_units_behind_the_terminator() {
        let mut h = Harness::running();
        let mut first = location();
        first.name = Some(wide("ABCDEFG"));
        tick(&mut h, FakeAuth { location: Some(first), ..good() });
        let mut second = location();
        second.name = Some(wide("AB"));
        tick(&mut h, FakeAuth { location: Some(second), ..good() });
        assert_eq!(text(h.server.allnet_auth().location_name()), "AB");
        assert_eq!(raw_units(&h, LOCATION_NAME, 8), [65, 66, 0, 68, 69, 70, 71, 0]);
    }

    #[test]
    fn empty_auth_text_overwrites_only_the_first_byte() {
        let mut h = Harness::running();
        tick(&mut h, FakeAuth { text: Some(b"token".to_vec()), ..good() });
        tick(&mut h, FakeAuth { text: None, ..good() });
        assert_eq!(&raw(&h)[AUTH_TEXT..AUTH_TEXT + 6], b"\0oken\0");
    }

    #[test]
    fn auth_text_is_cut_at_1535_characters() {
        let mut h = Harness::running();
        tick(&mut h, FakeAuth { text: Some(vec![b'T'; 1600]), ..good() });
        assert_eq!(h.server.allnet_auth().auth_text().to_bytes().len(), 1535);
        assert_eq!(raw(&h)[AUTH_TEXT + 1535], 0);
    }

    #[test]
    fn unflagged_or_invalid_last_auth_time_keeps_the_old_time() {
        let mut h = Harness::running();
        tick(&mut h, FakeAuth { time: Some(TimeSlot { valid: true, tm: tm(124, 4, 20, 1, 2, 3) }), ..good() });
        let published = h.server.allnet_auth().last_auth_time();
        assert_eq!(DateTime(published), civil(2024, 5, 20, 1, 2, 3));

        let unflagged = TimeSlot { valid: false, tm: tm(125, 0, 1, 0, 0, 0) };
        let month_13 = TimeSlot { valid: true, tm: tm(125, 12, 1, 0, 0, 0) };
        for slot in [unflagged, month_13] {
            tick(&mut h, FakeAuth { time: Some(slot), ..good() });
            assert_eq!(h.server.allnet_auth().last_auth_time(), published);
        }
    }

    #[test]
    fn publishes_every_ready_tick_without_a_clear_step_and_once_more_on_the_falling_edge() {
        let mut h = Harness::running();
        let mut slot = ControllerSlot::new(Box::new(AuthController::new(Box::new(good()))));
        h.server.span_mut(BLOCK + 2, 2).fill(0xEE);

        slot.pre_execute(&mut h.ctx());
        assert!(h.server.allnet_auth().good());
        assert_eq!(&raw(&h)[2..4], [0xEE; 2], "no clear step runs on the ready edge");

        h.manager.on_change_state(ProcessState(0));
        h.server.allnet_auth_mut().good_mut().set(false).expect("bool accepts true and false");
        slot.pre_execute(&mut h.ctx());
        assert!(h.server.allnet_auth().good(), "one final update on the falling edge");

        h.server.allnet_auth_mut().good_mut().set(false).expect("bool accepts true and false");
        slot.pre_execute(&mut h.ctx());
        assert!(!h.server.allnet_auth().good(), "nothing runs while not ready");
    }
}
