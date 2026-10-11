//! ALL.Net Wi-Fi units (alwifi and its UDP socket, `spec/libam/allnet.md` part 11).
//!
//! A unit announces itself with one JSON datagram on UDP port 40010; the cabinet keeps up to ten distinct units for
//! the lifetime each announced and exposes them as the unit list the `WiFiPort` reads. The wlanapi part of the
//! network module (`amyNetwork*`, part 11 section 4) belongs to `network`.
//!
//! # Cross-module methods
//!
//! * `WiFiPort::available`: `AuthConfig.enable && WifiConfig.enable`, what the facade reports.

mod port;

use crate::config::AllnetWifiConfig;
use serde_json::Value;

use crate::module::Cx;

pub use port::{WiFiPort, WifiHandle, WifiUnit};

/// Capacity of the unit table.
const MAX_UNITS: usize = 10;
/// Datagrams consumed per frame.
const DATAGRAMS_PER_FRAME: usize = 3;

/// One announced unit (the 80-byte `WifiInfo` record).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Announcement {
    serial: Vec<u8>,
    auth: bool,
    firm: Vec<u8>,
    server: bool,
    clients: u8,
    master: Vec<u8>,
    lifetime_s: u64,
    arrival_ms: u64,
}

impl Announcement {
    /// Parses one datagram (section 1.3); `None` drops it.
    fn parse(datagram: &[u8], now_ms: u64) -> Option<Self> {
        let Value::Object(object) = serde_json::from_slice::<Value>(datagram).ok()? else {
            return None;
        };
        let text = |key: &str, exact: Option<usize>, max: usize| -> Option<Vec<u8>> {
            let value = object.get(key)?.as_str()?;
            let ok = exact.map_or(value.len() <= max, |len| value.len() == len);
            ok.then(|| value.as_bytes().to_vec())
        };
        let integer = |key: &str, range: std::ops::RangeInclusive<i64>| -> Option<i64> {
            let value = object.get(key)?.as_i64()?;
            range.contains(&value).then_some(value)
        };
        let serial = text("serial", Some(12), 12)?;
        let auth = object.get("auth")?.as_bool()?;
        let firm = text("firm_version", None, 20)?;
        let server = object.get("wifi_server")?.as_bool()?;
        let clients = u8::try_from(integer("wifi_clients", 0..=31)?).ok()?;
        let master = match object.get("master_serial") {
            None => Vec::new(),
            Some(_) => text("master_serial", Some(12), 12)?,
        };
        let lifetime_s = u64::try_from(integer("lifetime", 1..=600)?).ok()?;
        Some(Self { serial, auth, firm, server, clients, master, lifetime_s, arrival_ms: now_ms })
    }

    /// The `WifiUnit` the facade shows: texts decoded from code page 932.
    fn to_unit(&self) -> WifiUnit {
        let mut unit = WifiUnit {
            auth_good: self.auth,
            server_alive: self.server,
            access_count: u64::from(self.clients),
            ..WifiUnit::default()
        };
        fill_wide(&mut unit.serial, &self.serial);
        fill_wide(&mut unit.firm, &self.firm);
        fill_wide(&mut unit.master, &self.master);
        unit
    }
}

/// Decodes `bytes` (code page 932) into the zeroed wide array `target`, leaving room for the terminator.
fn fill_wide(target: &mut [u16], bytes: &[u8]) {
    let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(bytes);
    let room = target.len().saturating_sub(1);
    for (slot, unit) in target.iter_mut().zip(decoded.encode_utf16().take(room)) {
        *slot = unit;
    }
}

/// The Wi-Fi unit side of `am::allnet::Manager`.
#[derive(Debug, Default)]
pub struct Wifi {
    config: AllnetWifiConfig,
    socket_open: bool,
    table: Vec<Announcement>,
    /// `LM+0x1A40`: the unit count the last refresh published.
    count: usize,
    units: Vec<WifiUnit>,
}

impl Wifi {
    /// `OperatingState::enter`: takes the Wi-Fi config.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.allnet_wifi;
        self.table.clear();
        self.units.clear();
        self.count = 0;
        self.socket_open = false;
    }

    /// Once per Operating frame, from [`super::Allnet::update`]: expires units, consumes up to three datagrams and
    /// refreshes the published list (`0x1403D1A60`, `0x14023BE30`).
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if !self.config.enable {
            return;
        }
        let wlan = &mut cx.backend.wlan;
        if !self.socket_open {
            self.socket_open = wlan.open();
            if !self.socket_open {
                return;
            }
        }
        let now = cx.clock.tick_ms();
        // Expiry: an entry that slid into a removed slot is examined only on the next poll.
        let mut index = 0;
        while index < self.table.len() {
            let unit = &self.table[index];
            if now.wrapping_sub(unit.arrival_ms) > 1000 * unit.lifetime_s {
                self.table.remove(index);
            }
            index += 1;
        }
        for _ in 0..DATAGRAMS_PER_FRAME {
            let Some(datagram) = wlan.receive() else { break };
            if datagram.is_empty() {
                continue;
            }
            let Some(announcement) = Announcement::parse(&datagram, now) else { continue };
            let known = self.table.iter().position(|known| known.serial == announcement.serial);
            match known {
                Some(slot) => self.table[slot] = announcement,
                None if self.table.len() < MAX_UNITS => self.table.push(announcement),
                None => tracing::warn!("wifi: the router table is full"),
            }
        }
        self.count = self.table.len();
        self.units = self.table.iter().map(Announcement::to_unit).collect();
    }

    /// Releases the Wi-Fi state: the socket closes and the table is cleared.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.socket_open {
            cx.backend.wlan.close();
        }
        self.socket_open = false;
        self.table.clear();
        self.units.clear();
        self.count = 0;
    }

    /// The Wi-Fi unit config.
    pub fn config(&self) -> &AllnetWifiConfig {
        &self.config
    }

    /// `0x14023D860`: the published unit list is emptied (10 records, count 0).
    pub fn clear_units(&mut self) {
        self.units.clear();
        self.count = 0;
    }

    /// The units libam found (`LM+0x1A40`).
    pub fn unit_count(&self) -> usize {
        self.count
    }

    /// Unit `index` of the published list.
    pub fn unit(&self, index: usize) -> Option<WifiUnit> {
        self.units.get(index).copied()
    }
}
