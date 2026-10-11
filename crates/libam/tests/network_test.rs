//! The network test sequence through `NetworkPort` on the simulated backend (`spec/libam/network.md` parts A, B).

use libam::Libam;
use libam::mock::{SIMULATED_RESOURCE_PATH, SimBackend};
use libam_api::am_config::WideStr;
use libam_api::interface::{AmInterface, CabinetAppConfig, LibamwConfig};
use libam_api::network::{NetworkPort, NetworkTestEvent, TestItem};
use tempfile::TempDir;

struct Cabinet {
    interface: Box<dyn AmInterface>,
    port: Box<dyn NetworkPort>,
    _root: TempDir,
}

fn cabinet() -> Cabinet {
    let root = tempfile::tempdir().expect("temporary directory");
    let (backend, sim) = SimBackend::new(root.path());
    let libam = Libam::new(backend, Box::new(sim.clock.clone()));
    let mut interface = libam.interface();
    interface.initialize(&LibamwConfig::default()).expect("initialize");
    for _ in 0..20 {
        let requested =
            interface.execute().iter().any(|e| matches!(e, libam_api::interface::LibamEvent::LoadCabinetType(_)));
        if requested {
            break;
        }
    }
    let mut config = CabinetAppConfig::default();
    config.am.common.resource.resource_path = WideStr::new(SIMULATED_RESOURCE_PATH).expect("resource path");
    config.am.common.resource.language = WideStr::new("en").expect("language");
    config.am.network.enable = true;
    config.am.network.use_gateway = true;
    config.am.network.use_location_router = true;
    config.controllers.network.test_event_listener = true;
    interface.load_cabinet_type_result(true, config);
    Cabinet { interface, port: libam.network(), _root: root }
}

impl Cabinet {
    fn run(&mut self, frames: usize) -> Vec<NetworkTestEvent> {
        let mut events = Vec::new();
        for _ in 0..frames {
            self.interface.execute();
            events.extend(self.port.take_test_events());
            // Get, DNS and traceroute finish on worker threads; a short wait lets the next frame observe them.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        events
    }
}

fn summary(events: &[NetworkTestEvent]) -> Vec<(bool, TestItem, u32, u32)> {
    events.iter().map(|e| (e.is_power_on, e.item, e.status.state, e.status.result)).collect()
}

#[test]
fn power_on_then_operator_test_run_every_item_in_order() {
    let mut cabinet = cabinet();
    assert!(cabinet.port.is_enabled());
    let power_on = cabinet.run(60);
    let items = summary(&power_on);
    // Arm events come first, item 0 running and items 1..3 waiting; disabled items are done with no result.
    assert_eq!(
        &items[..9],
        &[
            (true, TestItem::IpAddress, 1, 0),
            (true, TestItem::Gateway, 0, 0),
            (true, TestItem::DnsLan, 0, 0),
            (true, TestItem::Hops, 0, 0),
            (true, TestItem::LineType, 2, 0),
            (true, TestItem::AllnetAuth, 2, 0),
            (true, TestItem::Aime, 2, 0),
            (true, TestItem::Emoney, 2, 0),
            (true, TestItem::AimePay, 2, 0),
        ]
    );
    // Then every enabled checker finishes GOOD in item order and the whole test ends with item 9.
    assert_eq!(
        &items[9..],
        &[
            (true, TestItem::IpAddress, 2, 2),
            (true, TestItem::Gateway, 1, 0),
            (true, TestItem::Gateway, 2, 2),
            (true, TestItem::DnsLan, 1, 0),
            (true, TestItem::DnsLan, 2, 2),
            (true, TestItem::Hops, 1, 0),
            (true, TestItem::Hops, 2, 2),
            (true, TestItem::AllFinished, 2, 2),
        ]
    );
    let hops = power_on.iter().find(|e| e.item == TestItem::Hops && e.status.result == 2).expect("hops event");
    assert_eq!(hops.status.text_units(), "3".encode_utf16().collect::<Vec<_>>());
    assert!(cabinet.port.is_lan_available());
    assert!(cabinet.port.is_location_lan_available());
    let property = cabinet.port.property().expect("property after the IP check");
    assert_eq!(property.address.to_le_bytes(), [192, 168, 0, 5]);
    assert!(cabinet.port.is_item_enabled(TestItem::Hops));
    assert!(!cabinet.port.is_item_enabled(TestItem::Emoney));

    // An operator test restarts the checks; the power-on flag only clears on a forced stop (mode toggle).
    cabinet.port.begin_test();
    let started = cabinet.port.take_test_events();
    assert_eq!(started.len(), 9, "arm events are fired synchronously by begin_test");
    let rest = cabinet.run(60);
    let last = rest.last().expect("final event");
    assert_eq!((last.item, last.status.state, last.status.result), (TestItem::AllFinished, 2, 2));
}
