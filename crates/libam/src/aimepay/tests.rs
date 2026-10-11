//! Coin payment against a simulated Aime Pay server: the request sequence on the wire, the events in order, the
//! coin credit and the deal log persisted in the `AimePayInfo` record (`payment.md` PayApMgr 5.11, 6, 10).

use std::cell::{Cell, RefCell};
use std::fs;
use std::rc::Rc;

use crate::aime::AccessCode;
use crate::aimepay::AimePayEvent;
use crate::config::{AmConfig, WideStr};
use tempfile::TempDir;

use super::record::Info;
use super::*;
use crate::mock::{SimAimePayServer, SimBackend};
use crate::module::EventQueue;
use crate::nvram::NvramContext;
use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, RegValue};

const ITEM: &str = "1234567890123";
const AIME_ID: u32 = 12_345_678;
const RECEIPT: u64 = 777;
const CARD: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// The neighbour modules: a real display, a credit that records `add_coin`.
struct Neighbours {
    display: Display,
    coins: Vec<(u32, u32)>,
}

impl Links for Neighbours {
    fn display(&mut self) -> &mut Display {
        &mut self.display
    }
    fn display_ready(&self) -> bool {
        true
    }
    fn test_mode(&self) -> bool {
        false
    }
    fn ignore_coin(&self) -> bool {
        false
    }
    fn addable(&self, _player: u32) -> u32 {
        10
    }
    fn add_coin(&mut self, player: u32, coin: u32) -> bool {
        self.coins.push((player, coin));
        true
    }
    fn card_id(&self, aime_id: u32) -> Option<[u8; 8]> {
        (aime_id == AIME_ID).then_some(CARD)
    }
}

struct Rig {
    backend: Backend,
    sim: SimBackend,
    config: AmConfig,
    events: EventQueue,
    nvram: NvramContext,
    pay: AimePay,
    links: Neighbours,
    inputs: Inputs,
    requests: Rc<RefCell<Vec<(String, serde_json::Value)>>>,
    checked_in: Rc<Cell<bool>>,
    pay_answer: Rc<RefCell<String>>,
    _dir: TempDir,
}

impl Rig {
    /// A module with a stored shop, on a server that knows the same shop, run until deals are available.
    fn ready() -> Self {
        let dir = TempDir::new().unwrap();
        let (mut backend, sim) = SimBackend::new(dir.path());
        let env = dir.path().join("y").join("aimepay");
        fs::create_dir_all(&env).unwrap();
        fs::write(env.join("env.json"), r#"{"aimepay":{"root_endpoint":"http://127.0.0.1:1"}}"#).unwrap();

        let requests = Rc::default();
        let checked_in = Rc::new(Cell::new(true));
        let pay_answer = Rc::new(RefCell::new(format!(r#"{{"receipt_id":{RECEIPT},"amount":100}}"#)));
        backend.http = Box::new(SimAimePayServer {
            item_id: ITEM,
            log: Rc::clone(&requests),
            checked_in: Rc::clone(&checked_in),
            pay_answer: Rc::clone(&pay_answer),
        });

        let mut config = AmConfig::default();
        config.aimepay.enable = true;
        config.aimepay.resource_path = WideStr::new(r"Y:\aimepay").unwrap();
        config.aimepay.coin_item_id.push(WideStr::new(ITEM).unwrap()).unwrap();
        config.aimepay.app.complete_task_listener = true;
        config.aimepay.app.notify_deal_result_listener = true;
        config.aimepay.app.need_acceptation_listener = true;
        config.credit.rule.coin_amount = 100;
        config.credit.rule.coin_chute_multiplier = [1, 1];

        fs::create_dir_all(dir.path().join("y").join("amfs")).unwrap();
        sim.registry.set(MOUNT_KEY, "AMFS", RegValue::Text(r"Y:\amfs".to_owned()));
        let mut nvram = NvramContext::new();
        nvram.acquire(&mut backend).unwrap();
        let mut stored = Info::from_image(Info::zero_image());
        stored.set_location(&Location {
            store_code: "1234".into(),
            store_name: "Test Store".into(),
            company_code: "99".into(),
            company_name: "Test Corp".into(),
        });
        assert_eq!(nvram.store_record(Record::AimePayInfo, stored.image()), RecordStatus::Ok);

        let inputs = Inputs {
            prereq: true,
            net_ready: true,
            board: Some("A69E-01A88888888".into()),
            keychip: Some("A69E-01A99999999".into()),
            game_id: Some("SBXX".into()),
            client_id: 0x0A00_0001,
            ..Inputs::default()
        };
        let mut rig = Self {
            backend,
            sim,
            config,
            events: EventQueue::default(),
            nvram,
            pay: AimePay::new(),
            links: Neighbours { display: Display::new(), coins: Vec::new() },
            inputs,
            requests,
            checked_in,
            pay_answer,
            _dir: dir,
        };
        rig.with_cx(|pay, cx, _| pay.initialize(cx));
        for _ in 0..600 {
            if rig.pay.deal_available(true) {
                break;
            }
            rig.frame();
        }
        assert!(rig.pay.deal_available(true), "the module never became ready for deals");
        rig.requests.borrow_mut().clear();
        rig
    }

    fn with_cx<R>(&mut self, f: impl FnOnce(&mut AimePay, &mut Cx<'_>, &mut Neighbours) -> R) -> R {
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &self.sim.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &self.nvram,
        };
        f(&mut self.pay, &mut cx, &mut self.links)
    }

    fn frame(&mut self) {
        self.sim.clock.advance_ms(100);
        let inputs = self.inputs.clone();
        self.with_cx(|pay, cx, links| pay.update(cx, &inputs, links));
    }

    /// Frames until the task completes; every event in firing order.
    fn run_task(&mut self) -> Vec<AimePayEvent> {
        let mut events = Vec::new();
        for _ in 0..600 {
            self.frame();
            events.extend(self.pay.take_events());
            if events.iter().any(|event| matches!(event, AimePayEvent::CompleteTask { .. })) {
                return events;
            }
        }
        panic!("the task never completed: {events:?}");
    }

    fn paths(&self) -> Vec<String> {
        self.requests.borrow().iter().map(|(path, _)| path.clone()).collect()
    }

    fn start_coin_pay(&mut self) {
        let code = AccessCode([0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0x01, 0x23, 0x45, 0x67]);
        self.with_cx(|pay, cx, links| pay.coin_pay(cx, links, AIME_ID, &code, 0, 1)).expect("coin pay accepted");
    }

    fn stored_deals(&self) -> Vec<model::Deal> {
        let image = self.nvram.load_record(Record::AimePayInfo, Info::zero_image).unwrap();
        Info::from_image(image).entries().iter().map(Entry::to_deal).collect()
    }
}

#[test]
fn a_checked_in_user_pays_one_coin_end_to_end() {
    let mut rig = Rig::ready();
    rig.start_coin_pay();
    let events = rig.run_task();

    // The deal result precedes the completion of the task; nothing had to be accepted.
    assert!(matches!(
        events.as_slice(),
        [
            AimePayEvent::NotifyDealResult { task: 5, deal },
            AimePayEvent::CompleteTask { task: 5, error_kind: 0 },
        ] if deal.status == 0 && deal.item_count == 1
    ));

    assert_eq!(rig.paths(), ["/api/chkamp", "/api/payment/playfee", "/api/payment/success"]);
    let requests = rig.requests.borrow();
    assert_eq!(requests[1].1["aime_id"], AIME_ID);
    assert_eq!(requests[1].1["item_id"], ITEM);
    assert_eq!(requests[1].1["count"], 1);
    assert_eq!(requests[1].1["board_id"], "A69E01A8888");
    assert_eq!(requests[2].1["receipt_id"], RECEIPT);
    drop(requests);

    assert_eq!(rig.links.coins, [(0, 1)]);
    let deals = rig.stored_deals();
    assert_eq!(deals.len(), 1);
    assert_eq!((deals[0].status, deals[0].receipt_id, deals[0].amount), (0, RECEIPT, 100));
    assert_eq!(deals[0].item_id, ITEM);
    assert!(rig.pay.deal_available(true), "the module takes the next deal after the result display");
}

#[test]
fn a_user_who_is_not_checked_in_is_asked_for_the_pin_before_paying() {
    let mut rig = Rig::ready();
    rig.checked_in.set(false);
    rig.start_coin_pay();

    let mut events = Vec::new();
    for _ in 0..50 {
        rig.frame();
        events.extend(rig.pay.take_events());
        if !events.is_empty() {
            break;
        }
    }
    assert!(matches!(events.as_slice(), [AimePayEvent::NeedAcceptation { question: 2 }]));
    assert_eq!(rig.paths(), ["/api/chkamp"], "nothing is charged while the PIN is awaited");

    let pin: Vec<u16> = "4321".encode_utf16().collect();
    rig.with_cx(|pay, _, links| pay.confirm_with_param(links, &pin));
    let events = rig.run_task();

    assert_eq!(rig.paths(), ["/api/chkamp", "/api/chkpin", "/api/payment/playfee", "/api/payment/success"]);
    assert_eq!(rig.requests.borrow()[1].1["pin_code"], "4321");
    assert!(matches!(events.last(), Some(AimePayEvent::CompleteTask { task: 5, error_kind: 0 })));
    assert_eq!(rig.links.coins, [(0, 1)]);
}

#[test]
fn a_refused_payment_adds_no_coin_and_is_logged_as_failed() {
    let mut rig = Rig::ready();
    *rig.pay_answer.borrow_mut() = r#"{"result_code":20,"detail_code":"AMPEWL2050"}"#.to_owned();
    rig.start_coin_pay();
    let events = rig.run_task();

    assert!(matches!(
        events.as_slice(),
        [
            AimePayEvent::NotifyDealResult { task: 5, deal },
            AimePayEvent::CompleteTask { task: 5, error_kind: 1 },
        ] if deal.status == 1 && deal.error_code == 20
    ));
    assert_eq!(rig.paths(), ["/api/chkamp", "/api/payment/playfee"]);
    assert!(rig.links.coins.is_empty());
    let deals = rig.stored_deals();
    assert_eq!(deals.len(), 1);
    assert_eq!((deals[0].status, deals[0].error_code), (1, 20));
    assert_eq!(deals[0].error_message, "AMPEWL2050");
}
