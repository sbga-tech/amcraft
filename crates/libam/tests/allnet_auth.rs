//! A complete ALL.Net authentication (Auth 1.0 over a broadband line) against the simulated server, seen through
//! the `AuthPort` (`spec/libam/allnet.md` parts 3 to 5).

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::sync::Arc;

use backend::mock::tls::{Responder, Script};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use libam::Libam;
use libam::mock::{SIMULATED_RESOURCE_PATH, SimBackend};
use libam_api::allnet::AuthPort;
use libam_api::am_config::{AllnetAuthConfig, WideStr};
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use tempfile::TempDir;

const TENPO: Ipv4Addr = Ipv4Addr::new(10, 1, 0, 1);
const SERVER: Ipv4Addr = Ipv4Addr::new(10, 2, 0, 1);

struct Cabinet {
    interface: Box<dyn AmInterface>,
    auth: Box<dyn AuthPort>,
    sim: SimBackend,
    _root: TempDir,
}

impl Cabinet {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary directory");
        let (backend, sim) = SimBackend::new(root.path());
        let libam = Libam::new(backend, Box::new(sim.clock.clone()));
        Self { interface: libam.interface(), auth: libam.auth(), sim, _root: root }
    }

    fn start(&mut self, config: &AllnetAuthConfig) {
        self.interface.initialize(&LibamwConfig::default()).expect("initialize");
        for _ in 0..20 {
            let events = self.interface.execute();
            if events.iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_))) {
                break;
            }
        }
        let mut app = CabinetAppConfig::default();
        app.am.common.resource.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("resource path");
        app.am.common.resource.language = WideStr::new("en").expect("language");
        app.am.allnet_auth = config.clone();
        self.interface.load_cabinet_type_result(true, app);
    }

    /// Runs frames of 200 ms until the authentication is good.
    fn run_until_good(&mut self) {
        for _ in 0..400 {
            self.interface.execute();
            self.sim.clock.advance_ms(200);
            if self.auth.is_good() {
                return;
            }
        }
        panic!("authentication did not finish");
    }
}

/// The text of a power-on request: the compressed body behind the header.
fn request_text(request: &[u8]) -> (String, String) {
    let split = request.windows(4).position(|window| window == b"\r\n\r\n").expect("header end");
    let header = String::from_utf8_lossy(&request[..split]).into_owned();
    let body = std::str::from_utf8(&request[split + 4..]).expect("base64 body");
    let packed = STANDARD.decode(body).expect("base64");
    let mut text = String::new();
    ZlibDecoder::new(packed.as_slice()).read_to_string(&mut text).expect("zlib");
    (header, text)
}

/// The server's answer: echoes the request token, UTC 07:00 in the zone +0900.
fn answer(request: &[u8]) -> Vec<u8> {
    let (_, text) = request_text(request);
    let token = text.split('&').find_map(|pair| pair.strip_prefix("token=")).expect("token").trim_end();
    let body = format!(
        "stat=1&uri=http://naominet.jp/&host=naominet.jp&place_id=1234&name=Test%20Shop&nickname=Shop&region0=7\
         &region_name0=Tokyo&region_name1=&region_name2=&region_name3=&country=JPN&allnet_id=4321\
         &client_timezone=+0900&utc_time=2024-05-14T07:00:00Z&setting=&res_ver=3&token={token}&\r\n"
    );
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body.as_bytes()).expect("deflate");
    let packed = STANDARD.encode(encoder.finish().expect("deflate"));
    format!("HTTP/1.0 200 OK\r\nPragma: DFI\r\nContent-Length: {}\r\n\r\n{packed}\r\n", packed.len() + 2).into_bytes()
}

#[test]
fn power_on_exchange_authenticates_and_publishes_the_answer() {
    let mut cabinet = Cabinet::new();
    cabinet.sim.dns.set_host("tenporouter.loc", Some(TENPO));
    cabinet.sim.dns.set_host("bbrouter.loc", Some(TENPO));
    cabinet.sim.dns.set_host("naominet.jp", Some(SERVER));
    cabinet.sim.icmp.set_hops(TENPO, Some(2));
    cabinet.sim.adapters.set_local(Ipv4Addr::new(192, 168, 1, 10));
    cabinet.sim.tls.push_script(Script { responder: Some(Responder(Arc::new(answer))), ..Script::default() });

    cabinet.start(&AllnetAuthConfig { enable: true, support_line: 2, ..AllnetAuthConfig::default() });
    assert!(!cabinet.auth.is_good(), "nothing is authenticated before the first frames");
    cabinet.run_until_good();

    let connections = cabinet.sim.tls.connections();
    assert_eq!(connections.len(), 1, "one power-on connection: {connections:?}");
    let connection = &connections[0];
    assert_eq!((connection.addr, connection.port, connection.tls), (SERVER, 80, false));
    let (header, text) = request_text(&connection.written);
    assert!(header.starts_with("POST /sys/servlet/PowerOn HTTP/1.0\r\n"), "header: {header}");
    assert!(header.contains("\r\nPragma: DFI\r\n") && header.contains("Host: naominet.jp\r\n"), "header: {header}");
    let (query, token) = text.rsplit_once("&token=").expect("token field");
    // The identity is the simulated keychip (game id `SDAA`, id `A63E-01E12345678`) as the flow publishes it;
    // the application image gives no version.
    assert_eq!(
        query,
        "game_id=SDAA&ver=0.00&serial=A63E01E1234&ip=192.168.1.10&firm_ver=60001&boot_ver=0000&encode=UTF-8&format_ver=3&hops=2"
    );
    assert!(token.trim_end().parse::<u32>().is_ok(), "numeric token: {token:?}");

    assert!(cabinet.auth.is_good());
    assert_eq!(cabinet.auth.line_mask(), 2, "broadband line");
    let location = cabinet.auth.location().expect("location");
    assert_eq!(location.id, 1234);
    let name: Vec<u16> = location.name.expect("shop name").iter().copied().take_while(|&unit| unit != 0).collect();
    assert_eq!(String::from_utf16_lossy(&name), "Test Shop");
    let time = cabinet.auth.time().expect("authentication time");
    assert!(time.valid);
    assert_eq!((time.tm.year, time.tm.mon, time.tm.mday, time.tm.hour, time.tm.min), (124, 4, 14, 16, 0));
    assert!(cabinet.auth.text().is_none(), "Auth 1.0 has no auth_data text");
}
