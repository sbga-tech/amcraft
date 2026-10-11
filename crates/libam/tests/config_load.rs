//! Behaviour of `ConfigLoader::load` against JSON files in a temporary directory (`spec/libam/config.md`).

use std::fs;
use std::path::Path;

use libam::config::{FileConfigLoader, LoadError};
use libam_api::interface::{ConfigLoadRequest, ConfigLoader, LoadedConfig};
use tempfile::TempDir;

/// A temporary executable directory with config files.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("temporary directory") }
    }

    /// The executable directory string: the directory path with a trailing separator.
    fn exe_dir(&self) -> String {
        format!("{}/", self.dir.path().display())
    }

    /// Writes `name` into the directory and returns its full path.
    fn write(&self, name: &str, text: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(&path, text).expect("write config file");
        path.display().to_string()
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }
}

#[derive(Clone, Copy)]
struct Board {
    dip_switch: u16,
    model: u8,
    region: u32,
}

const BOARD: Board = Board { dip_switch: 0, model: 2, region: 1 };

fn load_with(
    fixture: &Fixture,
    files: &[String],
    board: Board,
    dump: &str,
) -> (FileConfigLoader, Option<LoadedConfig>) {
    let exe_dir = fixture.exe_dir();
    let request = ConfigLoadRequest {
        dip_switch: board.dip_switch,
        model: board.model,
        region: board.region,
        config_paths: files,
        exe_dir: &exe_dir,
        merged_dump_path: dump,
    };
    let mut loader = FileConfigLoader::new();
    let loaded = loader.load(&request);
    (loader, loaded)
}

/// Loads `files` and returns the loader that holds the result.
fn load(fixture: &Fixture, files: &[String]) -> (FileConfigLoader, Option<LoadedConfig>) {
    load_with(fixture, files, BOARD, "")
}

/// Loads one config text; the validation error text when it fails.
fn load_text(text: &str) -> Result<LoadedConfig, String> {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", text);
    let (loader, loaded) = load(&fixture, &[file]);
    loaded.ok_or_else(|| loader.error().expect("a failed load has an error").to_string())
}

fn error_of(text: &str) -> String {
    load_text(text).expect_err("the configuration must be rejected")
}

#[test]
fn default_path_is_config_json_in_the_exe_dir() {
    let fixture = Fixture::new();
    fixture.write("config.json", r#"{"common":{"max_player":3}}"#);
    let (loader, loaded) = load(&fixture, &[]);
    assert!(loaded.is_some());
    assert_eq!(loader.document().expect("document").common.max_player, 3);
}

#[test]
fn missing_default_file_cannot_be_opened() {
    let fixture = Fixture::new();
    let (loader, loaded) = load(&fixture, &[]);
    assert!(loaded.is_none());
    assert_eq!(loader.error(), Some(&LoadError::CannotOpen(fixture.path("config.json"))));
}

#[test]
fn explicit_files_replace_the_default_path() {
    let fixture = Fixture::new();
    fixture.write("config.json", "this is not json");
    let file = fixture.write("other.json", "{}");
    let (_, loaded) = load(&fixture, &[file]);
    assert!(loaded.is_some());
}

#[test]
fn language_default_follows_the_region() {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", "{}");
    for (region, language) in [(1, "japanese"), (2, "chinese"), (3, "english"), (0, "english")] {
        let board = Board { region, ..BOARD };
        let (loader, loaded) = load_with(&fixture, std::slice::from_ref(&file), board, "");
        assert!(loaded.is_some(), "region {region}");
        assert_eq!(loader.document().expect("document").common.language, language);
    }
    let file = fixture.write("b.json", r#"{"common":{"language":"klingon"}}"#);
    let (loader, _) = load(&fixture, &[file]);
    assert_eq!(loader.document().expect("document").common.language, "klingon");
}

#[test]
fn empty_config_has_the_documented_defaults() {
    let (loader, loaded) = {
        let fixture = Fixture::new();
        let file = fixture.write("a.json", "{}");
        let (loader, loaded) = load(&fixture, &[file]);
        (loader, loaded.expect("loads"))
    };
    let doc = loader.document().expect("document");
    assert_eq!(doc.common.max_player, 0);
    assert!(doc.sequence.save_total_time_on_playing);
    assert!(!doc.credit.enable);
    assert!(doc.abaas_log.use_network, "abaas_log.use_network defaults to true even without the section");
    assert_eq!(doc.aime.polling_interval, 100);
    assert!(!doc.air);
    assert!(!doc.apm);
    assert_eq!(loaded.bytes.air_enabled, 0);
    assert!(!loaded.process.debug_dump);
}

#[test]
fn present_section_without_enable_key_is_enabled() {
    let (loader, loaded) = {
        let fixture = Fixture::new();
        let file = fixture.write("a.json", r#"{"apm":{},"network":{"enable":true},"allnet_air":{"enable":false}}"#);
        let (loader, loaded) = load(&fixture, &[file]);
        (loader, loaded.expect("loads"))
    };
    let doc = loader.document().expect("document");
    assert!(doc.apm);
    assert!(!doc.air);
    assert_eq!(loaded.bytes.air_enabled, 0);
}

#[test]
fn later_files_override_scalars_and_append_arrays() {
    let fixture = Fixture::new();
    let a = fixture.write(
        "a.json",
        r#"{"common":{"max_player":1,"language":"first"},"usb_device":{"required":[{"id":["1:2"],"count":1}]}}"#,
    );
    let b = fixture
        .write("b.json", r#"{"common":{"max_player":2},"usb_device":{"required":[{"id":["abcd:ef"],"count":4}]}}"#);
    let (loader, loaded) = load(&fixture, &[a, b]);
    assert!(loaded.is_some());
    let doc = loader.document().expect("document");
    assert_eq!(doc.common.max_player, 2);
    assert_eq!(doc.common.language, "first", "a key the later file does not give is kept");
    let ids: Vec<_> = doc.usb_device.required.iter().map(|r| (r.ids.clone(), r.count)).collect();
    assert_eq!(ids, [(vec![(1, 2)], 1), (vec![(0xABCD, 0xEF)], 4)]);
}

#[test]
fn enable_false_in_a_later_file_stops_the_merge_of_that_section() {
    let fixture = Fixture::new();
    let a = fixture.write("a.json", r#"{"aime":{"unit":[{"port":1,"id":1}],"polling_interval":50}}"#);
    let b = fixture.write("b.json", r#"{"aime":{"enable":false,"polling_interval":70}}"#);
    let (loader, loaded) = load(&fixture, &[a, b]);
    assert!(loaded.is_some());
    let doc = loader.document().expect("document");
    assert!(!doc.aime.enable);
    assert!(doc.aime.units.is_empty(), "a disabled section reads no other key");
}

#[test]
fn condition_selects_files_by_model_dip_switch_and_region() {
    let fixture = Fixture::new();
    let files = [
        fixture.write("model.json", r#"{"condition":{"model_type":5},"common":{"max_player":1}}"#),
        fixture.write("region.json", r#"{"condition":{"region":"CHN"},"common":{"max_player":2}}"#),
        fixture.write("dip.json", r#"{"condition":{"dipsw":{"value":1,"mask":3}},"common":{"language":"dip"}}"#),
        fixture.write("never.json", r#"{"condition":{"model_type":6},"common":{"max_player":8,"language":"no"}}"#),
    ];
    let board = Board { dip_switch: 0x0101, model: 5, region: 2 };
    let (loader, loaded) = load_with(&fixture, &files, board, "");
    assert!(loaded.is_some());
    let doc = loader.document().expect("document");
    assert_eq!(doc.common.max_player, 2, "model.json and region.json match, the later one wins");
    assert_eq!(doc.common.language, "dip");

    // The same files for another board: the dip file (mask 3, value 1) still needs `dip & 3 == 1`.
    let board = Board { dip_switch: 0x0002, model: 6, region: 1 };
    let (loader, loaded) = load_with(&fixture, &files, board, "");
    assert!(loaded.is_some());
    let doc = loader.document().expect("document");
    assert_eq!(doc.common.max_player, 8);
    assert_eq!(doc.common.language, "no");
}

#[test]
fn no_matching_file_is_a_value_error() {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", r#"{"condition":{"model_type":9}}"#);
    let (loader, loaded) = load(&fixture, &[file]);
    assert!(loaded.is_none());
    assert_eq!(
        loader.error(),
        Some(&LoadError::InvalidValue("There is no file which matched in current board info.".to_owned()))
    );
}

#[test]
fn condition_errors_name_the_key() {
    let cases = [
        (r#"{"condition":{"region":"USA"}}"#, "Config error at \"condition.region\". invalid region. value = USA"),
        (r#"{"condition":{"jvs_node":5}}"#, "Config error at \"condition.jvs_node\". out of range. value = 5"),
        (r#"{"condition":{"model_type":99}}"#, "Config error at \"condition.model_type\". out of range. value = 99"),
        (r#"{"condition":{"dipsw":{"value":1}}}"#, "Config error at \"condition.dipsw.mask\". This key is required."),
    ];
    for (text, message) in cases {
        assert_eq!(error_of(text), message, "{text}");
    }
}

#[test]
fn invalid_model_type_is_fatal() {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", "{}");
    for model in [0, 0x63] {
        let board = Board { model, ..BOARD };
        let (loader, loaded) = load_with(&fixture, std::slice::from_ref(&file), board, "");
        assert!(loaded.is_none());
        assert_eq!(loader.error(), Some(&LoadError::InvalidModel(model)));
    }
}

#[test]
fn unreadable_and_malformed_files_stop_the_load() {
    let fixture = Fixture::new();
    let good = fixture.write("good.json", "{}");
    let broken = fixture.write("broken.json", "{\"common\": ");
    let missing = fixture.path("missing.json");

    let (loader, loaded) = load(&fixture, &[good.clone(), missing.clone()]);
    assert!(loaded.is_none());
    assert_eq!(loader.error(), Some(&LoadError::CannotOpen(missing)));

    let (loader, loaded) = load(&fixture, &[broken.clone(), good]);
    assert!(loaded.is_none());
    assert!(matches!(loader.error(), Some(LoadError::InvalidFormat(text)) if text.contains(&broken)));
}

#[test]
fn every_file_is_parsed_even_when_it_does_not_match() {
    let fixture = Fixture::new();
    let skipped = fixture.write("skipped.json", r#"{"condition":{"model_type":9}} trailing"#);
    let good = fixture.write("good.json", "{}");
    let (loader, loaded) = load(&fixture, &[skipped, good]);
    assert!(loaded.is_none());
    assert!(matches!(loader.error(), Some(LoadError::InvalidFormat(_))));
}

#[test]
fn merge_errors_are_fatal_and_name_file_and_path() {
    let fixture = Fixture::new();
    let file = fixture.write("b.json", r#"{"network":{"enable":1}}"#);
    let (loader, loaded) = load(&fixture, std::slice::from_ref(&file));
    assert!(loaded.is_none());
    assert_eq!(
        loader.error(),
        Some(&LoadError::Merge(format!(
            "Config merge error ({file}) at \"network.enable\". source type is not boolean."
        )))
    );

    let node_count = fixture.write("n.json", r#"{"condition":{"jvs_node":2},"jvs":{"node":[{"a":1}]}}"#);
    let (loader, _) = load(&fixture, std::slice::from_ref(&node_count));
    assert_eq!(
        loader.error(),
        Some(&LoadError::Merge(format!(
            "Config merge error ({node_count}) at \"jvs.node\". \
             source array size is not equals to \"condition.jvs_node\" (2)."
        )))
    );

    let players = fixture.write("p.json", r#"{"input":{"players":[{"a":1}]}}"#);
    let (loader, _) = load(&fixture, std::slice::from_ref(&players));
    assert_eq!(
        loader.error(),
        Some(&LoadError::Merge(format!(
            "Config merge error ({players}) at \"input.players[0]\". source type is not array."
        )))
    );
}

#[test]
fn validation_errors_have_the_documented_text() {
    let cases = [
        (r#"{"common":{"max_player":9}}"#, "Config error at \"common.max_player\". out of range. value = 9"),
        (r#"{"credit":{"max_credit":100}}"#, "Config error at \"credit.max_credit\". out of range. value = 100"),
        (r#"{"can":{}}"#, "Config error at \"can.port\". This key is required."),
        (
            r#"{"sequence":{"save_total_time_on_playing":1}}"#,
            "Config error at \"sequence.save_total_time_on_playing\". Invalid type. value = 1",
        ),
        (
            r#"{"common":{"language":"abcdefghijklmnop"}}"#,
            "Config error at \"common.language\". too large length. value = abcdefghijklmnop",
        ),
        (
            r#"{"sequence":{"play_accounting_timing":"later"}}"#,
            "Config error at \"sequence.play_accounting_timing\". invalid value. value = later",
        ),
        (
            r#"{"usb_device":{"required":[{"id":["xyz"],"count":1}]}}"#,
            "Config error at \"usb_device.required[0].id\". invalid id format. value = array",
        ),
    ];
    for (text, message) in cases {
        assert_eq!(error_of(text), message, "{text}");
    }
}

#[test]
fn first_failing_section_wins_in_the_fixed_order() {
    // `common` is validated before `credit`, and `error`-class sections before `can`.
    let text = r#"{"credit":{"max_credit":100},"common":{"max_player":9},"can":{}}"#;
    assert_eq!(error_of(text), "Config error at \"common.max_player\". out of range. value = 9");
    let text = r#"{"credit":{"max_credit":100},"can":{}}"#;
    assert_eq!(error_of(text), "Config error at \"can.port\". This key is required.");
}

#[test]
fn network_rules_between_sections() {
    let text = r#"{"network":{"enable":false},"lan_install":{"enable":true,"server":true}}"#;
    assert_eq!(error_of(text), "\"network.enable\" and \"lan_install.enable\" are inconsistent.");

    let text = r#"{"network":{"enable":true,"use_gateway":false},"lan_install":{"enable":true,"server":true}}"#;
    assert_eq!(error_of(text), "\"network.use_gateway\" and \"lan_install.enable\" are inconsistent.");

    // Without a network section the first enabled dependent section is reported (emoney before aimepay).
    let text = r#"{"allnet_air":{}}"#;
    assert_eq!(error_of(text), "\"network.enable\" and \"allnet_air.enable\" are inconsistent.");
}

#[test]
fn allnet_air_loads_but_is_flagged_for_the_callback() {
    let loaded = load_text(r#"{"network":{"enable":true},"allnet_air":{}}"#).expect("loads");
    assert_eq!(loaded.bytes.air_enabled, 1);
    assert!(loaded.process.allnet_air.enable);
    assert!(loaded.process.network.allnet_air_enable);
}

#[test]
fn allnet_rules_between_sections() {
    let text = r#"{"network":{"enable":true},"allnet_accounting":{"mode":"A"}}"#;
    let message = error_of(text);
    assert_eq!(message, "\"allnet_auth.enable\" and \"allnet_accounting.enable\" are inconsistent.");

    // An unknown accounting mode has an empty message.
    let text = r#"{"network":{"enable":true},"allnet_accounting":{"mode":"all"}}"#;
    assert_eq!(error_of(text), "Config error at \"allnet_accounting.mode\".  value = all");

    let text = r#"{"network":{"enable":true},"allnet_auth":{}}"#;
    assert_eq!(error_of(text), "A value of true is not contained in \"allnet_auth.support_line\".");
}

#[test]
fn com_ports_must_be_unique_across_sections() {
    let text = r#"{"aime":{"unit":[{"port":4,"id":1},{"port":4,"id":2}]}}"#;
    assert_eq!(error_of(text), "\"aime.unit[0].port\" and \"aime.unit[1].port\" are duplicated (COM4).");

    let text = r#"{"network":{"enable":true},
        "aime":{"unit":[{"port":4,"id":1}],"high_baudrate":true},
        "emoney":{"resource_path":"res","display_port":4}}"#;
    assert_eq!(error_of(text), "\"aime.unit[0].port\" and \"emoney.display_port\" are duplicated (COM4).");
}

#[test]
fn emoney_and_aimepay_fill_each_other() {
    let fixture = Fixture::new();
    let file = fixture.write(
        "a.json",
        r#"{"network":{"enable":true},
            "allnet_auth":{"support_line":{"broadband":true}},
            "aime":{"unit":[{"port":1,"id":1},{"port":2,"id":2}],"high_baudrate":true},
            "emoney":{"resource_path":"res","aime_unit":1},
            "aimepay":{"display_port":5}}"#,
    );
    let (loader, loaded) = load(&fixture, &[file]);
    let loaded = loaded.unwrap_or_else(|| panic!("load failed: {:?}", loader.error()));
    let doc = loader.document().expect("document");
    assert_eq!(doc.emoney.resource_path, format!("{}res", fixture.exe_dir()));
    assert_eq!(doc.aimepay.resource_path, doc.emoney.resource_path);
    assert_eq!((doc.emoney.display_port, doc.aimepay.display_port), (5, 5));
    assert_eq!((loaded.bytes.display_value_50974, loaded.bytes.display_value_50ca8), (5, 5));
    assert_eq!((loaded.bytes.display_flag_507e0, loaded.bytes.display_flag_50b14), (1, 1));
    assert_eq!(loaded.process.emoney.aime_unit, 1);
}

#[test]
fn emoney_and_aimepay_resource_paths_must_agree() {
    let base = |emoney: &str, aimepay: &str| {
        format!(
            r#"{{"network":{{"enable":true}},
                "allnet_auth":{{"support_line":{{"broadband":true}}}},
                "aime":{{"unit":[{{"port":1,"id":1}}],"high_baudrate":true}},
                "emoney":{{"resource_path":"{emoney}","display_port":2}},
                "aimepay":{{"resource_path":"{aimepay}","display_port":2}}}}"#
        )
    };
    assert!(load_text(&base("res", "./res")).is_ok(), "equal after normalisation");
    assert_eq!(
        error_of(&base("res", "other")),
        "\"emoney.resource_path\" and \"aimepay.resource_path\" are different."
    );
    assert_eq!(error_of(&base("", "")), "Both \"emoney.resource_path\" and \"aimepay.resource_path\" are empty.");
}

fn jvs_coin(slot: u32) -> String {
    format!(r#""jvs":{{"node":[[{{"required_function":{{"coin_input":{{"slot":{slot}}}}}}}]]}}"#)
}

const CAN_COIN: &str = r#""can":{"port":[{"number":1,"target":[
    {"product":"X","type":"io","id":1,"auto_reply":{"interval":1,"coin_input":{"slot":1}}}]}]}"#;

#[test]
fn only_one_board_may_provide_the_coin_input() {
    let text = format!("{{{},{CAN_COIN}}}", jvs_coin(1));
    assert_eq!(
        error_of(&text),
        "Both \"jvs.node[0][0].required_function.coin_input\" and \
         \"can.port[0].target[0].auto_reply.coin_input\" must not be enabled."
    );
}

#[test]
fn coin_selector_must_match_the_coin_slots_of_its_board() {
    let credit = |coins: &str| format!(r#""credit":{{"coin_selector_AS6DB":{{"port":3,"counted_coin":{coins}}}}}"#);

    let ok = format!("{{{},{}}}", jvs_coin(1), credit("[10]"));
    assert!(load_text(&ok).is_ok());

    let mismatch = format!("{{{},{}}}", jvs_coin(1), credit("[10,100]"));
    assert_eq!(
        error_of(&mismatch),
        "Config error at \"credit.coin_selector_AS6DB.counted_coin\". \
         Array size is not match the coin slot count of the JVS node."
    );

    let mismatch = format!("{{{CAN_COIN},{}}}", credit("[10,100]"));
    assert_eq!(
        error_of(&mismatch),
        "Config error at \"credit.coin_selector_AS6DB.counted_coin\". \
         Array size is not match the coin slot count of the CAN target."
    );

    let none = format!("{{{}}}", credit("[10]"));
    assert_eq!(
        error_of(&none),
        "Config error at \"credit.coin_selector_AS6DB\". \
         Coin input function is not enabled in all of the I/O board config."
    );
}

#[test]
fn credit_coin_boxes_are_spread_over_the_players() {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", r#"{"common":{"max_player":6},"credit":{}}"#);
    let (loader, loaded) = load(&fixture, &[file]);
    assert!(loaded.is_some());
    let boxes = loader.document().expect("document").credit.coin_box_assign;
    // `i / (((count - 1) >> 2) + 1)`: six players share the boxes two by two.
    assert_eq!(&boxes[..6], &[0, 0, 1, 1, 2, 2]);
}

#[test]
fn process_config_carries_what_the_controllers_read() {
    let loaded = load_text(
        r#"{"common":{"max_player":1,"log":{"output_request":true}},
            "sequence":{"save_total_time_on_playing":false},
            "usb_io":{"node":[{"id":0,"pwm_cycle":8,"required_function":{
                "pwm_output":{"slot":3},
                "unique_io":[{"command":65,"input":true,"output":false}]}}]}}"#,
    )
    .expect("loads");
    assert!(loaded.process.debug_dump);
    assert!(!loaded.process.sequence.save_total_time_on_playing);
    let nodes = &loaded.process.usb_io.nodes;
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0].pwm_enabled);
    assert_eq!(nodes[0].pwm_slots, 3);
    assert_eq!((nodes[0].uniques[0].code, nodes[0].uniques[0].input, nodes[0].uniques[0].output), (65, true, false));
}

#[test]
fn input_and_output_relations_are_tagged_and_split_by_player() {
    let loaded = load_text(
        r#"{"common":{"max_player":2},
            "input":{"system":[{"id":"test","type":"usb_switch","usb_node":0,"index":1}],
                     "players":[[{"id":"a","type":"usb_switch","usb_node":1,"index":4,"usb_player":0}],[]]}}"#,
    );
    // The relation formats are owned by the input module's own tests; here the map must at least load.
    assert!(loaded.is_ok(), "{loaded:?}");
}

#[test]
fn merged_dump_has_picojson_layout() {
    let fixture = Fixture::new();
    let a = fixture.write(
        "a.json",
        r#"{"condition":{"model_type":0},"ignored":1,
            "common":{"max_player":2,"language":"en/x"},
            "usb_device":{"required":[{"id":["1:2"],"count":1.5}]}}"#,
    );
    let dump = fixture.path("merged.json");
    let (_, loaded) = load_with(&fixture, &[a], BOARD, &dump);
    assert!(loaded.is_some());
    let expected = "{\n  \"common\": {\n    \"language\": \"en\\/x\",\n    \"max_player\": 2\n  },\n  \
                    \"usb_device\": {\n    \"required\": [\n      {\n        \"count\": 1.5,\n        \"id\": [\n          \
                    \"1:2\"\n        ]\n      }\n    ]\n  }\n}\n";
    assert_eq!(fs::read_to_string(dump).expect("dump file"), expected);
}

#[test]
fn merged_dump_renders_numbers_like_picojson() {
    let fixture = Fixture::new();
    let a = fixture.write(
        "a.json",
        r#"{"sequence":{"histogram_rule":[{"width":1e2,"count":3},{"width":86400.0,"count":1}],
                        "x":[1e22,0.1,-5,1.5e-7,123456789012345678]},
            "usb_device":{"required":[{"id":["1:1"],"count":1}]}}"#,
    );
    let dump = fixture.path("merged.json");
    let (_, loaded) = load_with(&fixture, &[a], BOARD, &dump);
    assert!(loaded.is_some());
    let text = fs::read_to_string(dump).expect("dump file");
    for number in ["100", "3", "86400", "1e+022", "0.10000000000000001", "-5", "1.4999999999999999e-007"] {
        assert!(
            text.contains(&format!(": {number}"))
                || text.contains(&format!("{number},\n"))
                || text.contains(&format!("{number}\n")),
            "{number} in\n{text}"
        );
    }
    assert!(text.contains("1.2345678901234568e+017"), "{text}");
}

#[test]
fn merged_dump_is_written_when_the_load_fails_but_not_after_a_merge_error() {
    let fixture = Fixture::new();
    let good = fixture.write("good.json", r#"{"common":{"max_player":1}}"#);
    let broken = fixture.write("broken.json", "[");
    let dump = fixture.path("merged.json");

    let (_, loaded) = load_with(&fixture, &[good, broken], BOARD, &dump);
    assert!(loaded.is_none());
    assert_eq!(
        fs::read_to_string(&dump).expect("dump of the files merged so far"),
        "{\n  \"common\": {\n    \"max_player\": 1\n  }\n}\n"
    );

    let invalid = fixture.write("invalid.json", r#"{"common":{"max_player":99}}"#);
    let (_, loaded) = load_with(&fixture, &[invalid], BOARD, &dump);
    assert!(loaded.is_none());
    assert!(fs::read_to_string(&dump).expect("dump").contains("99"), "the tree is dumped before validation");

    let merge_failure = fixture.write("merge.json", r#"{"network":{"enable":1}}"#);
    let second = fixture.path("second.json");
    let (_, loaded) = load_with(&fixture, &[merge_failure], BOARD, &second);
    assert!(loaded.is_none());
    assert!(!Path::new(&second).exists(), "a merge exception skips the dump");
}

#[test]
fn unwritable_dump_path_is_ignored() {
    let fixture = Fixture::new();
    let file = fixture.write("a.json", "{}");
    let dump = fixture.path("no-such-dir/merged.json");
    let (_, loaded) = load_with(&fixture, &[file], BOARD, &dump);
    assert!(loaded.is_some());
}

#[test]
fn jvs_node_tag_key_reaches_the_dump_but_not_the_condition() {
    let fixture = Fixture::new();
    let file = fixture.write(
        "a.json",
        r#"{"condition":{"jvs_node":2},"common":{"max_player":1},
            "input":{"system":[{"id":"x","type":"jvs_switch","node":0,"index":1}]}}"#,
    );
    let dump = fixture.path("merged.json");
    let (_, _) = load_with(&fixture, &[file], BOARD, &dump);
    let text = fs::read_to_string(dump).expect("dump");
    assert!(text.contains("__jvs_node_count."), "{text}");
    assert!(!text.contains("condition"), "{text}");
}
