use super::*;
use crate::mock::SimBackend;
use backend::Backend;
use backend::mock::SimRegistry;

fn text(registry: &SimRegistry, value: &str) -> Option<String> {
    match registry.get(MASTER_KEY, value)? {
        RegValue::Text(text) => Some(text),
        other => panic!("unexpected registry value {other:?}"),
    }
}

#[test]
fn error_message_becomes_system_error_line() {
    let mut registry = SimRegistry::new();
    let result = exit_request(&mut registry, 1, Some("ERROR 0023\nMain board = bad?"));
    assert_eq!(result, 0);
    assert_eq!(text(&registry, "NextProcess").unwrap(), r"s:\sgsegaboot.exe");
    assert_eq!(
        text(&registry, "SystemError").unwrap(),
        "system_error=enable&error_number=23&message=Main@_board@_@-@_bad@.&category=error\r\n"
    );
    assert!(registry.is_volatile(MASTER_KEY));
}

#[test]
fn message_with_forbidden_character_resets_next_process() {
    let mut registry = SimRegistry::new();
    let result = exit_request(&mut registry, 1, Some("ERROR 0023\nbad: text"));
    assert_eq!(result, exit_result::INVALID_PARAMETER);
    assert_eq!(text(&registry, "NextProcess").unwrap(), "");
    assert_eq!(text(&registry, "SystemError"), None);
}

#[test]
fn message_without_error_number_is_rejected_after_next_process_was_set() {
    let mut registry = SimRegistry::new();
    assert_eq!(exit_request(&mut registry, 3, Some("ERROR 12\nx")), exit_result::INVALID_PARAMETER);
    // The next-process value of step 3 is not rolled back by the missing-number path.
    assert_eq!(text(&registry, "NextProcess").unwrap(), r"s:\sgsegaboot.exe -s test");
}

#[test]
fn five_digit_numbers_above_u16_are_skipped() {
    assert_eq!(parse_system_message("E 70000 0042\nm"), Some((42, "m")));
    assert_eq!(parse_system_message("E 65535\nm"), Some((65535, "m")));
}

#[test]
fn long_message_and_unknown_codes() {
    let mut registry = SimRegistry::new();
    let long = format!("ERROR 0001\n{}", "a".repeat(200));
    assert_eq!(exit_request(&mut registry, 1, Some(&long)), exit_result::MESSAGE_TOO_LONG);
    assert_eq!(exit_request(&mut registry, 6, None), exit_result::INVALID_PARAMETER);
    assert_eq!(text(&registry, "NextProcess"), None);
}

#[test]
fn line_longer_than_255_units_is_rejected() {
    // Code 3 avoids the code-1 limit of 200 units; with a 4-digit number the escaped message may have at most
    // 192 characters (B.5).
    let mut registry = SimRegistry::new();
    let fits = format!("ERROR 1234\n{}", "a".repeat(192));
    assert_eq!(exit_request(&mut registry, 3, Some(&fits)), 0);
    let too_long = format!("ERROR 1234\n{}", "a".repeat(193));
    assert_eq!(exit_request(&mut registry, 3, Some(&too_long)), exit_result::INVALID_PARAMETER);
}

fn finalized(master: &mut Master) -> (SimRegistry, Backend) {
    let dir = std::env::temp_dir();
    let (mut backend, sim) = SimBackend::new(dir);
    master.finalize(&mut backend);
    (sim.registry, backend)
}

#[test]
fn finalize_maps_inputs_to_launcher_values() {
    // mode 1 (Auto) with the test flag -> exit code 3, no system error.
    let mut master = Master::new();
    master.attach();
    master.set_next_process(mode::AUTO);
    master.set_test_flag(true);
    let (registry, _keep) = finalized(&mut master);
    assert_eq!(text(&registry, "NextProcess").unwrap(), r"s:\sgsegaboot.exe -s test");
    assert_eq!(text(&registry, "SystemError").unwrap(), "system_error=disable\r\n");

    // mode 2 with the download-UI flag -> exit code 5 and the empty error message.
    let mut master = Master::new();
    master.attach();
    master.set_next_process(mode::SEGA_BOOT);
    master.set_download_ui(true);
    let (registry, _keep) = finalized(&mut master);
    assert_eq!(text(&registry, "NextProcess").unwrap(), r"c:\windows\sega\system\sgxdownloadui.exe");
    assert_eq!(
        text(&registry, "SystemError").unwrap(),
        "system_error=enable&error_number=0&message=&category=error\r\n"
    );

    // mode 3 carries the latched fatal error.
    let mut master = Master::new();
    master.attach();
    master.set_next_process(mode::SEGA_BOOT_ERROR);
    master.copy_fatal_error(4104, "Invalid config");
    let (registry, _keep) = finalized(&mut master);
    assert_eq!(
        text(&registry, "SystemError").unwrap(),
        "system_error=enable&error_number=4104&message=Invalid@_config&category=error\r\n"
    );
}

#[test]
fn detached_master_ignores_writers_and_writes_nothing() {
    let mut master = Master::new();
    master.set_next_process(mode::SEGA_BOOT);
    let (registry, _keep) = finalized(&mut master);
    assert_eq!(text(&registry, "NextProcess"), None);

    // Unknown modes (5 and above) clear the next process.
    let mut master = Master::new();
    master.attach();
    master.set_next_process(7);
    let (registry, _keep) = finalized(&mut master);
    assert_eq!(text(&registry, "NextProcess").unwrap(), "");
    assert_eq!(text(&registry, "SystemError").unwrap(), "system_error=disable\r\n");
}

#[test]
fn app_loader_count_reads_the_launcher_dword() {
    let mut registry = SimRegistry::new();
    assert_eq!(Master::app_loader_count(&mut registry), 0);
    registry.set(MASTER_KEY, "AppLoaderCount", RegValue::Dword(3));
    assert_eq!(Master::app_loader_count(&mut registry), 3);
}
