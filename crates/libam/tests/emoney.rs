//! The e-money module through `EMoneyPort` on the simulated backend (`spec/libam/payment.md`): a store card
//! activates the terminal, an amount payment runs against the scripted SDK, and the events and the deal log show
//! what the original delivers to the game.

use backend::HttpResponse;
use backend::mock::SimHttp;
use backend::mock::thinca::{Callback, SimThinca};
use backend::thinca::{SdkDeal, SdkDetail, SdkResult, kind};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use libam::Libam;
use libam::emoney::StoreCard;
use libam::mock::devices::{SimAimeReader, SimCard, encrypt_card};
use libam::mock::{SIMULATED_RESOURCE_PATH, SimBackend, SimClock};
use libam_api::aime::AimePort;
use libam_api::am_config::{AimeUnit, WideStr};
use libam_api::emoney::{EMoneyAppConfig, EMoneyEvent, EMoneyPort};
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam_api::result::AmError;
use md5::{Digest, Md5};
use serde_json::json;
use tempfile::TempDir;

const DEAL_NUMBER: &str = "DEAL00000000001";
const CARD_NUMBER: &str = "7777000011112222";

struct Cabinet {
    interface: Box<dyn AmInterface>,
    port: Box<dyn EMoneyPort>,
    aime_port: Box<dyn AimePort>,
    clock: SimClock,
    http: SimHttp,
    sdk: SimThinca,
    aime_reader: SimAimeReader,
    events: Vec<EMoneyEvent>,
    _root: TempDir,
}

/// A cabinet whose e-money module reads the store card through the Aime reader unit of the simulated COM port 1.
fn cabinet() -> Cabinet {
    let root = tempfile::tempdir().expect("temporary directory");
    let (backend, sim) = SimBackend::new(root.path());
    // The environment of the e-money module: the activation URL and the obfuscated certificate password.
    let env = json!({"tasms": {"root_endpoint": "https://sms.example/root"}, "thincapayment": {"ca_file": "secret"}});
    std::fs::write(root.path().join("r").join("res").join("env.json"), env.to_string()).expect("env.json");

    let sdk = sim.thinca.clone();
    sdk.add_brand(1, "Brand One", "one.png");
    sdk.add_brand(2, "Brand Two", "two.png");
    let (http, clock, aime_reader) = (sim.http.clone(), sim.clock.clone(), sim.serial.aime_reader(1));

    let libam = Libam::new(backend, Box::new(sim.clock.clone()));
    let mut interface = libam.interface();
    interface.initialize(&LibamwConfig::default()).expect("initialize");
    for _ in 0..20 {
        if interface.execute().iter().any(|event| matches!(event, LibamEvent::LoadCabinetType(_))) {
            break;
        }
    }
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config.am.network.enable = true;
    config.am.network.use_gateway = true;
    config.am.network.use_location_router = true;
    config.display_flag = true;
    config.display_value = 2;
    config.sinks.display_complete = true;
    config.am.aime.enable = true;
    config.am.aime.high_baudrate = true;
    config.am.aime.units.push(AimeUnit { com_port: 1, unit_index: 1, flag: false }).expect("one reader");
    config.controllers.aime.task_finished_listener = true;
    config.controllers.aime.need_acceptation_listener = true;
    config.am.emoney.enable = true;
    config.am.emoney.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("e-money resource path");
    config.controllers.emoney = EMoneyAppConfig {
        complete_task_listener: true,
        notify_deal_result_listener: true,
        play_sound_listener: true,
        notify_cancel_listener: true,
        hold_card_listener: true,
    };
    interface.load_cabinet_type_result(true, config);
    let (port, aime_port) = (libam.emoney(), libam.aime());
    Cabinet { interface, port, aime_port, clock, http, sdk, aime_reader, events: Vec::new(), _root: root }
}

impl Cabinet {
    fn frames(&mut self, count: usize) {
        for _ in 0..count {
            self.clock.advance_ms(20);
            self.interface.execute();
            self.events.extend(self.port.take_events());
        }
    }

    /// Frames until `done` holds, at most `limit`.
    fn run_until(&mut self, limit: usize, done: impl Fn(&Self) -> bool) {
        for _ in 0..limit {
            if done(self) {
                return;
            }
            self.frames(1);
        }
        assert!(
            done(self),
            "condition not reached; events so far: {:?}; sdk calls: {:?}",
            self.events,
            self.sdk.calls()
        );
    }

    fn take(&mut self) -> Vec<EMoneyEvent> {
        std::mem::take(&mut self.events)
    }
}

fn http_ok(status: u16, headers: Vec<(String, String)>, body: String) -> Result<HttpResponse, backend::HttpError> {
    Ok(HttpResponse { status, headers, body: body.into_bytes() })
}

/// What the sales-management site answers to the activation request, and then to the alive run that follows.
fn script_site(http: &SimHttp) {
    let certificate = b"client certificate".to_vec();
    let digest: String = Md5::digest(&certificate).iter().map(|byte| format!("{byte:02x}")).collect();
    let settings = json!({
        "endpoints": {
            "terminals": {"uri": "https://sms.example/terminals"},
            "statuses": {"uri": "https://sms.example/statuses"},
            "sales": {"uri": "https://sms.example/sales"},
            "counters": {"uri": "https://sms.example/counters"}
        },
        "intervals": {"checkSetting": 15, "sendStatus": 30},
        "settigsType": "AmusementTerminalSettings",
        "status": "10",
        "terminalId": "123456789012345678901234567890",
        "version": "2024-05-01T09:00:00",
        "availableElectronicMoney": [1, 2],
        "cashAvailability": false,
        "productCode": 7
    });
    let body = json!({"certificate": STANDARD.encode(&certificate), "initSettings": settings}).to_string();
    http.push_answer(http_ok(200, vec![("x-certificate-md5".into(), digest)], body));
    http.push_answer(http_ok(304, vec![], String::new()));
    http.push_answer(http_ok(204, vec![], String::new()));
    http.push_answer(http_ok(204, vec![], String::new()));
}

fn card_fields() -> StoreCard {
    StoreCard {
        store_card_id: "0123456789ABCDEF".into(),
        merchant_code: "M0000000000000000001".into(),
        branch: "00123".into(),
        aux: "AUXAUXAUXAUXAUXX".into(),
    }
}

/// Puts the store card of the tests on the simulated Aime reader (id bytes 1..=7, kind 2).
fn place_store_card(reader: &SimAimeReader) {
    const ID: [u8; 7] = [1, 2, 3, 4, 5, 6, 7];
    let card = encrypt_card(&ID, 2, &card_fields());
    reader.place_card(SimCard::Store { id: ID.to_vec(), blocks: card.blocks });
}

fn payment_result() -> SdkResult {
    SdkResult {
        code: 0,
        brand: 1,
        text: DEAL_NUMBER.into(),
        brands: vec![],
        deal: Some(SdkDeal { card: CARD_NUMBER.into(), balance_after: 700 }),
        details: vec![SdkDetail {
            kind: 1,
            type_: 1,
            status: 0,
            has_balance: 1,
            time: None,
            card: CARD_NUMBER.into(),
            amount: 300,
            balance_before: 1000,
            balance_after: 700,
        }],
        kind: kind::PAYMENT,
    }
}

/// A blank terminal is activated with a store card; an amount payment of 300 yen then runs against the SDK and
/// is completed by the game. `NotifyDealResult` precedes `CompleteTask`, and the deal log keeps the result.
#[test]
fn activated_terminal_pays_an_amount_and_logs_the_deal() {
    let mut cabinet = cabinet();
    assert!(cabinet.port.is_enabled());
    cabinet.frames(200);

    // Activation: the reader, the site (activate, check setting, counters, status) and `initAuthTerm`.
    script_site(&cabinet.http);
    place_store_card(&cabinet.aime_reader);
    cabinet.sdk.script_burst(vec![Callback::Finished(SdkResult {
        code: 0,
        brands: vec![1, 2],
        kind: kind::INIT_AUTH_TERM,
        ..SdkResult::default()
    })]);
    let before = cabinet.aime_reader.requests().len();
    cabinet.port.terminal_auth().expect("terminalAuth starts");
    cabinet.run_until(400, |cabinet| {
        cabinet.events.iter().any(|event| matches!(event, EMoneyEvent::CompleteTask { task: 1, .. }))
    });
    let completions: Vec<_> = cabinet
        .take()
        .into_iter()
        .filter_map(|event| match event {
            EMoneyEvent::CompleteTask { task, status } => Some((task, status)),
            _ => None,
        })
        .collect();
    assert_eq!(completions, [(1, 0)], "the activation completes with status 0");
    let sent = cabinet.aime_reader.requests();
    assert!(sent.iter().skip(before).any(|frame| frame.request.cmd == 0x40), "the card read started the reader");
    // `stops()` recorded each stop's `red` flag, not the blue blink polling starts with. Off is nine zero bytes;
    // a refused card is solid red (`255, 0, 0, ...`).
    let stops: Vec<_> = sent
        .iter()
        .skip(before)
        .filter(|frame| frame.request.cmd == 0x82)
        .skip(1)
        .map(|frame| frame.request.payload.clone())
        .collect();
    assert_eq!(stops, vec![vec![0; 9]], "an accepted card does not blink red");
    assert!(cabinet.port.terminal_id().is_some(), "the activated terminal has an id");

    // The payment.
    cabinet.run_until(400, |cabinet| cabinet.port.can_operate(4));
    cabinet.sdk.script_burst(vec![
        Callback::DealId(DEAL_NUMBER.into()),
        Callback::Cancellable(false),
        Callback::Finished(payment_result()),
    ]);
    let brand = cabinet.port.brand(0).expect("an authenticated brand");
    let item_id: Vec<u16> = "1234567890123".encode_utf16().collect();
    cabinet.port.amount_pay(300, 1, brand, &item_id).expect("amountPay starts");
    // The amount is confirmed by the SDK (status 4) but stays pending until the game completes it: the deal is in
    // the log and no event has fired yet.
    cabinet.run_until(400, |cabinet| cabinet.port.deal_log_count() == 1);
    assert_eq!(cabinet.port.deal_log_item(0).expect("pending deal").status, 4);
    assert!(cabinet.events.is_empty(), "nothing is announced before completePay: {:?}", cabinet.events);
    cabinet.port.complete_pay().expect("completePay");
    cabinet.frames(5);

    let events = cabinet.take();
    let notified = events
        .iter()
        .position(|event| matches!(event, EMoneyEvent::NotifyDealResult { task: 4, .. }))
        .expect("NotifyDealResult(4)");
    let completed = events
        .iter()
        .position(|event| matches!(event, EMoneyEvent::CompleteTask { task: 4, status: 0 }))
        .expect("CompleteTask(4, 0)");
    assert!(notified < completed, "the deal is announced before the task completes: {events:?}");

    let deal = cabinet.port.deal_log_item(0).expect("the deal log entry");
    assert_eq!(cabinet.port.deal_log_count(), 1);
    assert_eq!(deal.status, 0, "completePay turns the pending status 4 into 0");
    assert_eq!((deal.amount, deal.balance_before, deal.balance_after), (300, 1000, 700));
    let number: String = char::decode_utf16(deal.deal_number.iter().copied().take_while(|&unit| unit != 0))
        .map(|unit| unit.unwrap_or('?'))
        .collect();
    assert_eq!(number, DEAL_NUMBER);
    let log_card: String = char::decode_utf16(deal.card_number.iter().copied().take_while(|&unit| unit != 0))
        .map(|unit| unit.unwrap_or('?'))
        .collect();
    assert_eq!(log_card, CARD_NUMBER);
}

/// The store card is read through the Aime reader unit e-money is configured with: the simulated reader of COM port
/// 1 holds a MIFARE card whose sectors open with key A `thinca`, and the activation completes with status 0. The
/// reader receives exactly the seven unit commands of PayEmSvc 6.4 in their order: `StartPolling`, the blue LED
/// blink, `CardDetect`, the key, one authentication per sector with its three block reads (the key diversifier is
/// the last four bytes of the 7-byte card ID), `StopPolling`, the LED off.
#[test]
fn activation_reads_the_store_card_through_the_aime_unit() {
    let mut cabinet = cabinet();
    cabinet.frames(200);

    script_site(&cabinet.http);
    cabinet.sdk.script_burst(vec![Callback::Finished(SdkResult {
        code: 0,
        brands: vec![1, 2],
        kind: kind::INIT_AUTH_TERM,
        ..SdkResult::default()
    })]);
    place_store_card(&cabinet.aime_reader);

    let before = cabinet.aime_reader.requests().len();
    cabinet.port.terminal_auth().expect("terminalAuth starts");
    // While e-money reads the card it holds the reader lease: the Aime module does not touch the unit.
    cabinet.run_until(400, |cabinet| {
        cabinet.aime_reader.requests().iter().skip(before).any(|sent| sent.request.cmd == 0x40)
    });
    assert_eq!(cabinet.aime_port.set_led(0, 0xFF, 0, 0), Err(AmError::code(AmError::BUSY)));
    cabinet.run_until(400, |cabinet| {
        cabinet.events.iter().any(|event| matches!(event, EMoneyEvent::CompleteTask { task: 1, .. }))
    });
    let completions: Vec<_> = cabinet
        .take()
        .into_iter()
        .filter_map(|event| match event {
            EMoneyEvent::CompleteTask { task, status } => Some((task, status)),
            _ => None,
        })
        .collect();
    assert_eq!(completions, [(1, 0)], "the activation completes with status 0");
    assert!(cabinet.port.terminal_id().is_some(), "the activated terminal has an id");
    assert_eq!(
        cabinet.aime_port.set_led(0, 0xFF, 0, 0),
        Ok(()),
        "the Aime module has the unit back once the activation is over"
    );

    // `(destination, command, payload)`: the unit commands go to the reader (unit 1), the LED frames to its LED
    // board (unit address | 8).
    let wire: Vec<(u8, u8, Vec<u8>)> = cabinet
        .aime_reader
        .requests()
        .into_iter()
        .skip(before)
        .filter(|sent| sent.com == 1)
        .map(|sent| (sent.request.dst, sent.request.cmd, sent.request.payload))
        .collect();
    let on_card = |block: u8| (1, 0x52, vec![4, 5, 6, 7, block]);
    let authenticate = |trailer: u8| (1, 0x51, vec![4, 5, 6, 7, trailer]);
    let expected = vec![
        (1, 0x40, vec![0x01]),
        (9, 0x82, vec![0, 0, 255, 0, 0, 0x01, 0xF4, 0x01, 0xF4]),
        (1, 0x42, vec![]),
        (1, 0x50, b"thinca".to_vec()),
        authenticate(7),
        on_card(4),
        on_card(5),
        on_card(6),
        authenticate(11),
        on_card(8),
        on_card(9),
        on_card(10),
        authenticate(15),
        on_card(12),
        on_card(13),
        on_card(14),
        (1, 0x41, vec![]),
        (9, 0x82, vec![0; 9]),
    ];
    assert_eq!(wire, expected);
}
