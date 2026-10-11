use std::io::Read as _;
use std::net::Ipv4Addr;

use crate::config::AmConfig;
use flate2::read::DeflateDecoder;

use super::*;
use crate::clock::Clock as _;
use crate::mock::SimBackend;
use crate::module::EventQueue;
use backend::mock::tls::{Script, SimTls};
use backend::tls::TlsProvider as _;

fn http_reply(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

fn link() -> AuthLink {
    AuthLink {
        done: true,
        good: true,
        server: Some(Ipv4Addr::LOCALHOST),
        tenpo: Some(Ipv4Addr::new(10, 1, 2, 3)),
        identity: Identity {
            keychip: "12345678901".into(),
            board: "09876543210".into(),
            game_id: "SBZZ".into(),
            game_version: "1.00A".into(),
            function_type: 1,
        },
        date_synced: true,
        bias_minutes: -540,
        operating_mode: 1,
        ..AuthLink::default()
    }
}

/// Splits a request into its head text and the inflated body text.
fn decode(request: &[u8]) -> (String, String) {
    let split = request.windows(4).position(|window| window == b"\r\n\r\n").expect("head end");
    let head = String::from_utf8(request[..split].to_vec()).expect("ascii head");
    let mut body = String::new();
    DeflateDecoder::new(&request[split + 4..]).read_to_string(&mut body).expect("raw deflate body");
    (head, body)
}

#[test]
fn the_daily_report_sends_the_trace_log_and_schedules_the_next_day() {
    let root = tempfile::tempdir().expect("temp dir");
    let (mut backend, sim) = SimBackend::new(root.path());
    let tls = SimTls::new();
    tls.push_script(Script {
        reply: http_reply("result=0&requestno=0&traceerase=1&waittime=0\r\n"),
        ..Script::default()
    });
    backend.tls = Box::new(tls.clone());
    let keychip = sim.keychip.clone();

    let mut config = AmConfig::default();
    config.allnet_accounting.mode = 1;
    let mut events = EventQueue::default();
    let mut billing = Billing::default();
    let link = link();
    let nvram = crate::nvram::NvramContext::new();
    let mut cx = Cx { backend: &mut backend, clock: &sim.clock, events: &mut events, config: &config, nvram: &nvram };
    billing.initialize(&mut cx);
    settle(&mut billing, &mut cx, &link);
    assert_eq!(billing.phase(), Phase::Idle);
    assert!(billing.accounting_available());

    // One TLS connection to the auth server on port 8443, with the server name and the compressed request.
    let connections = tls.connections();
    assert_eq!(connections.len(), 1);
    assert_eq!((connections[0].addr, connections[0].port), (Ipv4Addr::LOCALHOST, 8443));
    assert_eq!(connections[0].server_name, "ib.naominet.jp");
    let (head, body) = decode(&connections[0].written);
    assert!(
        head.starts_with(
            "POST /request/ HTTP/1.1\r\nContent-Type: application/octet-stream\r\nHost: ib.naominet.jp\r\n"
        )
    );
    assert!(head.contains("\r\nConnection: close\r\n"));
    assert!(head.contains("\r\nCache-Control: no-cache\r\n"));

    let civil = sim.clock.local_now().civil();
    let date = schedule::stamp_of_civil(civil).digits();
    let mut lines = body.split("\r\n");
    assert_eq!(
        lines.next(),
        Some(
            format!(
                "keychipid=12345678901&functype=1&gameid=SBZZ&gamever=1.00A&boardid=09876543210&tenpoip=10.1.2.3\
                 &libalibver=1.314&datamax=7154&billingtype=0&protocolver=1.000&operatingfix=0&traceleft=0\
                 &requestno=0&datesync=1&timezone=900&date={date}&crcerrcnt=0&memrepair=0&playcnt=0\
                 &playlimit=1000&nearfull=10"
            )
            .as_str()
        )
    );
    // The credit configuration and the "Auth start" message are the two entries of the log.
    let entries: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
    assert_eq!(entries.len(), 2);
    assert!(entries[0].starts_with("tt=2&rn=0&sn=0&"), "{}", entries[0]);
    assert!(entries[1].starts_with("tt=1&rn=1&sn=1&") && entries[1].ends_with("&me=Auth start"), "{}", entries[1]);

    // `traceerase=1`: the acknowledged entries are gone; the erase note and "Auth OK" are the new log.
    let log = keychip.trace_records();
    assert_eq!(log.len(), 2);
    assert!(log.iter().all(|record| record[0] & 0x7F == 1));
    assert!(String::from_utf8_lossy(&log[0][7..39]).starts_with("EraseTD 12345678901 SBZZ 1.00A"));
    assert!(String::from_utf8_lossy(&log[1][7..39]).starts_with("Auth OK"));

    // The next daily report: tomorrow at the (not yet re-applied, so 00:00) start time plus the default delay.
    let delay = schedule::default_delay_s(Ipv4Addr::new(10, 1, 2, 3), "SBZZ");
    let expected = schedule::next_fg_time(civil, 0, 0, delay).expect("valid month") - schedule::seconds_of(civil);
    assert_eq!(i64::from(billing.reporter.seconds_until(schedule::seconds_of(civil))), expected);
    let report_time = reporter::load_report_time_from(&*cx.backend.fs).expect("last_pras.log");
    assert_eq!(
        (report_time.year + 1900, report_time.mon + 1, report_time.mday),
        (
            i32::from(schedule::stamp_of_civil(civil).year),
            i32::from(schedule::stamp_of_civil(civil).month),
            i32::from(schedule::stamp_of_civil(civil).day)
        )
    );
    assert!(cx.backend.fs.map(r"C:\Users\AppUser\temp\first_ar.conf").exists());
    assert!(tls.open().peer_chain().is_empty());
}

#[test]
fn a_restore_requested_by_the_settings_file_is_reported_as_memrepair() {
    let root = tempfile::tempdir().expect("temp dir");
    let (mut backend, sim) = SimBackend::new(root.path());
    let tls = SimTls::new();
    tls.push_script(Script {
        reply: http_reply("result=0&requestno=0&traceerase=0&waittime=0\r\n"),
        ..Script::default()
    });
    backend.tls = Box::new(tls.clone());
    let keychip = sim.keychip.clone();
    let settings = backend.fs.map(r"C:\Users\AppUser\temp\alib.conf");
    std::fs::create_dir_all(settings.parent().expect("temp dir")).expect("create the temp dir");
    std::fs::write(&settings, "is_restored=0\n").expect("settings file");

    let mut config = AmConfig::default();
    config.allnet_accounting.mode = 1;
    let mut events = EventQueue::default();
    let mut billing = Billing::default();
    let link = link();
    let nvram = crate::nvram::NvramContext::new();
    let mut cx = Cx { backend: &mut backend, clock: &sim.clock, events: &mut events, config: &config, nvram: &nvram };
    billing.initialize(&mut cx);
    settle(&mut billing, &mut cx, &link);
    assert_eq!(billing.phase(), Phase::Idle);

    assert_eq!(keychip.restore_count(), 1);
    let marker = cx.backend.fs.map(r"C:\Users\AppUser\temp\is_restored.conf");
    assert_eq!(std::fs::read_to_string(marker).expect("is_restored.conf"), "is_restored=1\n");
    let (_, body) = decode(&tls.connections()[0].written);
    let query = body.split("\r\n").next().expect("query line");
    assert!(query.contains("&crcerrcnt=0&memrepair=1&playcnt=0&"), "{query}");
}

#[test]
fn a_credit_runs_through_start_play_and_end_and_counts_one_play() {
    let root = tempfile::tempdir().expect("temp dir");
    let (mut backend, sim) = SimBackend::new(root.path());
    let tls = SimTls::new();
    tls.push_script(Script { reply: http_reply("result=0&requestno=0&waittime=0\r\n"), ..Script::default() });
    backend.tls = Box::new(tls);
    let keychip = sim.keychip.clone();

    let mut config = AmConfig::default();
    config.allnet_accounting.mode = 1;
    let mut events = EventQueue::default();
    let mut billing = Billing::default();
    let link = link();
    let nvram = crate::nvram::NvramContext::new();
    let mut cx = Cx { backend: &mut backend, clock: &sim.clock, events: &mut events, config: &config, nvram: &nvram };
    billing.initialize(&mut cx);
    settle(&mut billing, &mut cx, &link);
    let slot = billing.begin_accounting(0, 1, 1).expect("credit accepted");
    for _ in 0..10 {
        billing.update(&mut cx, &link);
    }
    billing.end_accounting(0, 1, 1, 1, slot).expect("credit ended");
    for _ in 0..10 {
        billing.update(&mut cx, &link);
    }
    assert_eq!(keychip.play_count(), 1);
    assert_eq!(billing.queued_requests(), 0);
}

/// Advances frames, pausing so the report worker can finish, until billing is idle or the wait gives up.
fn settle(billing: &mut Billing, cx: &mut Cx<'_>, link: &AuthLink) {
    let start = std::time::Instant::now();
    while billing.phase() != Phase::Idle {
        billing.update(cx, link);
        if start.elapsed() > std::time::Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
