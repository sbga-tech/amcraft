//! `ConfigLoader::convert`: a representative config file is loaded and converted, and the resulting libam
//! `AmConfig` is checked section by section (`spec/libam/config.md`, "Conversion driver").

use std::fs;

use libam::config::FileConfigLoader;
use libam_api::am_config::{AmConfig, Pair, UNSET_U32, UNSET_U64};
use libam_api::interface::{CabinetAppConfig, ConfigLoadRequest, ConfigLoader, LoadedConfig};
use tempfile::TempDir;

const CONFIG: &str = r#"{
    "common": {"max_player": 2},
    "error": {"auto_reset": true},
    "sequence": {
        "save_total_time_on_playing": false,
        "histogram_rule": [{"width": 10, "count": 2}, {"width": 60, "count": 1}]
    },
    "video": {
        "mode": "dual",
        "monitor": [{"width": 1920, "height": 1080}, {"width": 1280, "height": 720, "angle": 90}]
    },
    "backup": {"clear_busram_at_first_launch": true},
    "jvs": {"node": [[{"required_function": {"switch_input": {"player": 2, "button": 8}}}]]},
    "usb_io": {
        "firmware_path": "fw/usbio.bin",
        "node": [{
            "id": 3,
            "timeout": 100,
            "pwm_cycle": 8,
            "required_function": {
                "pwm_output": {"slot": 3},
                "unique_io": [{"command": 65, "param": [7], "input": true}]
            }
        }]
    },
    "credit": {"max_credit": 50, "config": {"freeplay": false, "coin_to_credit": 2}},
    "network": {
        "enable": true,
        "location_router_directly": "allow",
        "property": {"save": false, "dhcp": false, "ip_address": "192.168.0.10", "subnet_mask": "255.255.255.0",
                     "gateway": "192.168.0.1", "primary_dns": "", "secondary_dns": ""}
    },
    "allnet_auth": {
        "support_line": {"broadband": true, "xdsl": true},
        "develop_version": "1.23",
        "type": "2.0",
        "log": {"level": 3}
    },
    "allnet_accounting": {"mode": "B1", "log": {"level": 3}},
    "allnet_wifi": {},
    "aime": {
        "unit": [{"port": 1, "id": 1}, {"port": 2, "id": 2}],
        "high_baudrate": true,
        "polling_interval": 50,
        "firmware_path": ["fw/a.bin", "fw/b.bin"],
        "log": {"root_path": "aime/log"}
    },
    "emoney": {
        "resource_path": "emres",
        "display_port": 3,
        "aime_unit": 1,
        "log": {"level": 2, "root_path": "emoney/log"},
        "ignore_brand": ["ID", "nanaco"]
    },
    "aimepay": {"resource_path": "emres", "display_port": 3, "coin_item_id": ["COIN", "COIN2"]},
    "abaas_log": {"root_path": "abaas/log", "use_network": false, "log": {"level": 4}},
    "apm": {}
}"#;

struct Loaded {
    _dir: TempDir,
    exe_dir: String,
    loader: FileConfigLoader,
    loaded: LoadedConfig,
}

fn load(text: &str) -> Loaded {
    let dir = tempfile::tempdir().expect("temporary directory");
    let file = dir.path().join("config.json");
    fs::write(&file, text).expect("write config");
    let exe_dir = format!("{}/", dir.path().display());
    let files = [file.display().to_string()];
    let request = ConfigLoadRequest {
        dip_switch: 0,
        model: 2,
        region: 1,
        config_paths: &files,
        exe_dir: &exe_dir,
        merged_dump_path: "",
    };
    let mut loader = FileConfigLoader::new();
    let loaded = loader.load(&request);
    let loaded = loaded.unwrap_or_else(|| panic!("load failed: {}", loader.error().expect("error")));
    Loaded { _dir: dir, exe_dir, loader, loaded }
}

/// Runs `convert` the way the cabinet-type callback does: the auth level byte is the merged maximum.
fn convert(source: &mut Loaded) -> (bool, CabinetAppConfig) {
    let bytes = &mut source.loaded.bytes;
    bytes.byte_4ff8c = bytes.byte_4ff8c.max(bytes.byte_4ff98).max(bytes.byte_4ff9d);
    let mut app_config = CabinetAppConfig::default();
    let ok = source.loader.convert(&source.loaded, &source.exe_dir, &mut app_config);
    (ok, app_config)
}

fn converted(source: &mut Loaded) -> (AmConfig, CabinetAppConfig) {
    let (ok, app_config) = convert(source);
    assert!(ok, "convert failed: {:?}", source.loader.convert_error());
    (app_config.am.clone(), app_config)
}

#[test]
fn representative_config_fills_every_part() {
    let mut source = load(CONFIG);
    let exe_dir = source.exe_dir.clone();
    let (am, app_config) = converted(&mut source);

    assert_eq!(am.common.max_player, 2);
    assert_eq!(am.common.resource.resource_path.as_str(), format!("{exe_dir}am_resources\\"));
    assert_eq!(am.common.resource.language.as_str(), "japanese");
    assert!(am.error.auto_reset && !am.error.raise_in_test);
    assert_eq!(am.sequence.histogram.as_slice(), [10, 10, 60]);
    assert!(!am.sequence.save_total_time_on_playing);

    assert!(am.video.enable);
    assert_eq!(am.video.mode, 2);
    assert_eq!((am.video.resolution.width, am.video.resolution.height, am.video.angle), (1920, 1080, 0));
    assert_eq!(
        (am.video.secondary_resolution.width, am.video.secondary_resolution.height, am.video.secondary_angle),
        (1280, 720, 90)
    );
    assert!(am.nvram.clear_busram_at_first_launch && !am.nvram.clear_eeprom_at_first_launch);

    // The JVS list of one node is libam list 1 and the selected one; every list is marked valid.
    assert!(am.jvs.enable);
    assert_eq!(am.jvs.default_total, 1);
    assert!(am.jvs.totals.iter().all(|total| total.valid));
    let node = &am.jvs.totals[0].nodes.as_slice()[0];
    assert_eq!(node.switch_input, Some(Pair { a: 2, b: 8 }));
    assert!(node.auto_output_zero && node.coin_input.is_none());
    assert!(am.jvs.totals[1].nodes.is_empty());

    assert!(am.usbio.enable);
    assert_eq!(am.usbio.firmware_path.as_str(), "fw\\usbio.bin");
    let node = &am.usbio.nodes.as_slice()[0];
    assert_eq!((node.id, node.comm_timeout, node.switch_sampling, node.pwm_cycle), (3, Some(100), None, Some(8)));
    assert_eq!(node.funcs.pwm_output, Some(3));
    let unique = node.funcs.unique_io.as_slice()[0];
    assert_eq!((unique.command, unique.params, unique.param_count), (65, [7, 0, 0], 3));

    // Credit: unset values stay unset, the share flags and the free-play flag are mapped to 1 / 2.
    assert!(am.credit.enable);
    assert_eq!(am.credit.max_credit, 50);
    assert_eq!(am.credit.coin_box_assign[7], UNSET_U64);
    assert_eq!(am.credit.rule.operation, 1);
    assert_eq!((am.credit.rule.chute_share_type, am.credit.rule.service_share_type), (0, 0));
    assert_eq!((am.credit.rule.coin_to_credit, am.credit.rule.bonus_adder), (2, UNSET_U32));

    assert!(am.network.enable && am.network.use_gateway);
    assert_eq!(am.network.location_router_directly, 2);
    let property = am.network.property.expect("static property");
    assert!(!property.dhcp);
    assert_eq!(property.address, 0x0A00_A8C0);
    assert_eq!(property.subnet_mask, 0x00FF_FFFF);
    assert_eq!((property.gateway, property.primary_dns), (0x0100_A8C0, 0));
    assert!(am.network.dhcp_if_unset, "`save: false` is stored negated");
    // Disabled sections keep the libam defaults: lan_install writes only its interval, net_delivery its flags.
    assert_eq!((am.laninstall.mode, am.laninstall.interval), (0, 20));
    assert!(!am.netdeliver.enable && am.netdeliver.const_one);

    // The support-line bits skip bit 0, the log byte is the merged maximum (3), `4` maps to 0.
    assert!(am.allnet_auth.enable);
    assert_eq!(am.allnet_auth.support_line, 2 | 8);
    assert_eq!(am.allnet_auth.multi_auth_method, 3);
    assert_eq!(am.allnet_auth.auth_type, 1);
    assert!(am.allnet_auth.version.valid);
    assert_eq!(am.allnet_auth.version.packed, (1 << 16) | (23 << 8));
    assert_eq!(am.allnet_auth.version.long_text, "1.23.00");
    assert_eq!(am.allnet_auth.version.short_text, "1.23");
    assert_eq!(am.allnet_accounting.mode, 2);
    assert!(am.allnet_wifi.enable);

    assert!(am.aime.enable && am.aime.high_baudrate);
    assert_eq!(am.aime.polling_interval, 50);
    assert_eq!(am.aime.card_accept_time, 2_147_483);
    let units: Vec<_> = am.aime.units.iter().map(|unit| (unit.com_port, unit.unit_index)).collect();
    assert_eq!(units, [(1, 1), (2, 2)]);
    let firmware: Vec<_> = am.aime.firmware_paths.iter().map(|path| path.as_str()).collect();
    assert_eq!(firmware, ["fw\\a.bin", "fw\\b.bin"]);
    assert!(am.aime.log_enable);
    assert_eq!(am.aime.log_root_path.as_str(), "aime\\log\\");

    assert!(am.emoney.enable);
    assert_eq!(am.emoney.root_path.as_str(), exe_dir);
    assert_eq!(am.emoney.resource_path.as_str(), format!("{exe_dir}emres"));
    assert_eq!((am.emoney.log_level, am.emoney.aime_unit), (2, 1));
    assert_eq!(am.emoney.log_root_path.as_str(), "emoney\\log\\");
    // `iD` is libam brand 3 and `Nanaco` brand 1; the others stay allowed.
    assert_eq!(am.emoney.brand_enabled, [true, false, true, false, true, true, true, true]);

    assert!(am.aimepay.enable);
    assert_eq!(am.aimepay.resource_path.as_str(), format!("{exe_dir}emres"));
    let ids: Vec<_> = am.aimepay.coin_item_id.iter().map(|id| id.as_str()).collect();
    assert_eq!(ids, ["COIN", "COIN2"]);

    // abaas_log: level 4 is accepted by the validator and mapped to 0; the root keeps its trailing backslash and
    // is also handed to the daemon.
    assert!(am.abaas_log.enable && !am.abaas_log.use_network);
    assert_eq!(am.abaas_log.level, 0);
    assert_eq!(am.abaas_log.root_path.as_str(), "abaas\\log\\");
    assert_eq!(app_config.abaas_log_root.as_deref(), Some("abaas\\log\\"));
    assert!(am.apm.enable);
}

#[test]
fn disabled_sections_keep_the_libam_defaults() {
    let mut source = load(
        r#"{"network":{"enable":false,"use_gateway":false},"lan_install":{"enable":false,"server":false,"interval":50}}"#,
    );
    let (am, app_config) = converted(&mut source);
    let defaults = AmConfig::default();

    // Only the enable flag of a disabled network section is written: `use_gateway` stays the libam default.
    assert_eq!(am.network, defaults.network);
    assert!(am.network.use_gateway);
    // The lan_install interval is written whatever `enable` says.
    assert_eq!((am.laninstall.mode, am.laninstall.interval), (0, 20));
    assert_eq!(am.aime, defaults.aime);
    assert_eq!(am.emoney, defaults.emoney);
    assert_eq!(am.abaas_log, defaults.abaas_log);
    assert_eq!(app_config.abaas_log_root, None);
    // A disabled jvs section does not select a list.
    assert_eq!(am.jvs, defaults.jvs);
}

#[test]
fn lan_install_client_is_one_and_server_is_two() {
    let client = r#"{"network":{"enable":true},"lan_install":{"server":false}}"#;
    let server = r#"{"network":{"enable":true},"lan_install":{"server":true,"interval":30}}"#;
    for (text, mode, interval) in [(client, 1, 20), (server, 2, 30)] {
        let (am, _) = converted(&mut load(text));
        assert_eq!((am.laninstall.mode, am.laninstall.interval), (mode, interval));
    }
}

#[test]
fn a_converter_failure_fails_the_conversion() {
    // A 260-unit firmware path passes the validator (limit 261) but not libam's setter (needs fewer than 260).
    let path = "f".repeat(260);
    let mut source = load(&format!(
        r#"{{"usb_io":{{"firmware_path":"{path}","node":[{{"id":0,"required_function":{{"coin_input":{{"slot":1}}}}}}]}}}}"#
    ));
    let (ok, _) = convert(&mut source);
    assert!(!ok);
    assert_eq!(source.loader.convert_error().expect("error").converter, "usb_io");

    // An enabled jvs section without a node cannot select a list.
    let mut source = load(r#"{"jvs":{"node":[]}}"#);
    let (ok, _) = convert(&mut source);
    assert!(!ok);
    assert_eq!(source.loader.convert_error().expect("error").converter, "jvs");
}

#[test]
fn convert_without_a_loaded_document_fails() {
    let mut loader = FileConfigLoader::new();
    let mut app_config = CabinetAppConfig::default();
    assert!(!loader.convert(&LoadedConfig::default(), "C:\\app\\", &mut app_config));
}
