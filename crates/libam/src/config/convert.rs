//! The Config -> `am::AppConfig` conversion (`0x140006380`, `spec/libam/config.md` "Conversion driver" and the
//! converter section of every part).
//!
//! The 21 converters run in the original's fixed order against the loaded [`Config`]; each one fills its part of
//! [`AmConfig`]. A converter that fails stops the run (`false` for the caller); what earlier converters wrote
//! stays, as in the original where they write into the one `AppConfig` object. Sections with an `enable` flag
//! write only that flag when it is off: the rest keeps the libam default.

use std::fmt;

use super::am_config::{
    self as am, AllnetVersion, AmConfig, BoundedList, CanBoard, CanNode, JvsNode as AmJvsNode, Pair, UNSET_U32,
    UNSET_U64, UniqueCmd, WideStr,
};
use crate::interface::CabinetAppConfig;
use crate::usbio::UsbRequired;

use super::Config;

/// Why a converter failed: its name and the libam setter's complaint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    /// The converter (the section it converts).
    pub converter: &'static str,
    /// What the libam setter refused.
    pub reason: String,
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} conversion failed: {}", self.converter, self.reason)
    }
}

impl std::error::Error for ConvertError {}

type Outcome = Result<(), ConvertError>;

/// Everything a converter reads.
struct Source<'a> {
    config: &'a Config,
    exe_dir: &'a str,
    /// `Config[0x4FF8C]` after the callback's max-merge of the three ALL.Net log levels.
    auth_log_level: u8,
}

type Converter = fn(&Source<'_>, &mut AmConfig) -> Outcome;

/// The converters in the order of the driver's vector.
const CONVERTERS: [Converter; 21] = [
    common,
    error,
    sequence,
    video,
    backup,
    jvs,
    can,
    usb_io,
    credit,
    usb_device,
    network,
    lan_install,
    net_delivery,
    auth,
    accounting,
    wifi,
    aime,
    emoney,
    aime_pay,
    abaas_log,
    apm,
];

/// Runs the 21 converters, then derives the abaas root the callback hands to the Manager. Stops at the first
/// failing converter.
pub(super) fn convert(
    config: &Config,
    auth_log_level: u8,
    exe_dir: &str,
    app_config: &mut CabinetAppConfig,
) -> Outcome {
    let source = Source { config, exe_dir, auth_log_level };
    for converter in CONVERTERS {
        converter(&source, &mut app_config.am)?;
    }
    // The daemon's controllers own the `UsbDeviceAppConfig` that `effective()` folds in; it must carry the sets
    // the converter produced, as the original keeps both in one object.
    if app_config.controllers.usb_device.required().is_empty() {
        for set in app_config.am.usbdevice.app.required() {
            app_config.controllers.usb_device.add_required(set.clone()).map_err(|error| fail("usb_device", error))?;
        }
    }
    // Step 11 of the callback: the root is only passed on when `abaas_log.enable` is set.
    app_config.abaas_log_root = app_config.am.abaas_log.enable.then(|| app_config.am.abaas_log.root_path.to_string());
    Ok(())
}

fn fail(converter: &'static str, reason: impl fmt::Display) -> ConvertError {
    ConvertError { converter, reason: reason.to_string() }
}

/// A `wchar_t[N]` setter: fails when the text does not fit.
fn wide<const N: usize>(converter: &'static str, text: &str) -> Result<WideStr<N>, ConvertError> {
    WideStr::new(text).map_err(|error| fail(converter, error))
}

/// An "append" setter: fails when the list is full.
fn append<T, const N: usize>(converter: &'static str, list: &mut BoundedList<T, N>, item: T) -> Outcome {
    list.push(item).map_err(|error| fail(converter, error))
}

/// `0x14005BAB0`: the path ends in a separator; an empty path becomes `.\`.
fn ensure_separator(path: &str) -> String {
    match path.chars().last() {
        None => ".\\".to_owned(),
        Some('\\' | '/') => path.to_owned(),
        Some(_) => format!("{path}\\"),
    }
}

/// `0x140227F70`: a trailing `\` is appended to a non-empty path that does not end in one.
fn terminate_path(path: &str) -> String {
    if path.is_empty() || path.ends_with('\\') { path.to_owned() } else { format!("{path}\\") }
}

/// `0x14005B950` followed by [`ensure_separator`]: `base` + `relative` + `\`.
fn join_directory(base: &str, relative: &str) -> String {
    let relative = relative.trim_start_matches(['\\', '/']);
    let mut path = base.to_owned();
    if !path.is_empty() && !path.ends_with(['\\', '/']) {
        path.push('\\');
    }
    path.push_str(relative);
    ensure_separator(&path)
}

/// 1: `CommonConverter` (`0x140010110`).
fn common(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.common;
    // The qword store happens first; the resource block is only committed when all of it succeeded.
    out.common.max_player = u64::from(section.max_player);
    let resource_path = wide::<260>("common", &join_directory(src.exe_dir, "am_resources"))?;
    let language = wide::<16>("common", &section.language)?;
    let mut custom_resource = BoundedList::default();
    for id in section.custom_resource_ids.iter().filter(|id| !id.is_empty()) {
        append("common", &mut custom_resource, wide::<16>("common", id)?)?;
    }
    out.common.resource = am::CommonResource { resource_path, language, custom_resource };
    Ok(())
}

/// 2: `ErrorConverter` (`0x140011050`).
fn error(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    out.error.auto_reset = src.config.error.auto_reset;
    out.error.raise_in_test = src.config.error.raise_in_test;
    Ok(())
}

/// 3: `SequenceConverter` (`0x140011B20`). `play_accounting_timing` is not converted.
fn sequence(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.sequence;
    for rule in &section.histogram_rules {
        // `0x140213A40`: a zero count is "Invalid total"; the list holds 32 widths.
        if rule.count == 0 {
            return Err(fail("sequence", "Invalid total"));
        }
        for _ in 0..rule.count {
            append("sequence", &mut out.sequence.histogram, rule.width)?;
        }
    }
    out.sequence.save_total_time_on_playing = section.save_total_time_on_playing;
    Ok(())
}

/// 4: `VideoConverter` (`0x140012900`).
fn video(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.video;
    out.video.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    out.video.mode = section.mode as u32;
    let [primary, secondary] = section.resolutions;
    out.video.resolution = am::Resolution { width: primary.width, height: primary.height };
    out.video.angle = primary.angle;
    out.video.secondary_resolution = am::Resolution { width: secondary.width, height: secondary.height };
    out.video.secondary_angle = secondary.angle;
    out.video.ignore_unconnected = section.ignore_unconnected;
    Ok(())
}

/// 5: `BackupConverter` (`0x14000FAC0`).
fn backup(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    out.nvram.clear_eeprom_at_first_launch = src.config.backup.clear_eeprom_at_first_launch;
    out.nvram.clear_busram_at_first_launch = src.config.backup.clear_busram_at_first_launch;
    Ok(())
}

/// 6: Jvs converter (`0x140011180`). The node group of length `k + 1` becomes libam list `k + 1`; the lowest
/// list that holds a node is the selected one.
fn jvs(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.jvs;
    out.jvs.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    for (total, nodes) in out.jvs.totals.iter_mut().zip(&section.lists) {
        // The list setter marks every list valid, empty or not.
        total.valid = true;
        for node in nodes {
            let function = &node.required_function;
            let converted = AmJvsNode {
                share_coin_slot: node.share_coin_slot,
                auto_output_zero: node.auto_output_zero,
                switch_input: function.switch_input.map(|s| Pair { a: u64::from(s.player), b: u64::from(s.button) }),
                coin_input: function.coin_input.map(u64::from),
                analog_input: function.analog_input.map(|a| Pair { a: u64::from(a.channel), b: u64::from(a.bit) }),
                rotary_input: function.rotary_input.map(u64::from),
                general_driver: function.general_driver.map(u64::from),
            };
            append("jvs", &mut total.nodes, converted)?;
        }
    }
    let selected = section.lists.iter().position(|nodes| !nodes.is_empty());
    out.jvs.default_total = selected.map_or(0, |index| index as u64 + 1);
    if selected.is_none() {
        return Err(fail("jvs", "no set JVS node config"));
    }
    Ok(())
}

/// 7: `CanConverter` (`0x14000FBD0`).
fn can(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.can;
    out.can.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    for port in &section.ports {
        let mut board = CanBoard { number: u64::from(port.number), nodes: BoundedList::default() };
        for target in &port.targets {
            let reply = &target.auto_reply;
            let mut mask = 0;
            for (bit, set) in [reply.switch_input, reply.analog_input_a, reply.analog_input_b, reply.rotary_input]
                .into_iter()
                .enumerate()
            {
                mask |= u32::from(set) << bit;
            }
            let node = CanNode {
                target: am::CanTarget {
                    // Config type 1..=4 (motor, sensor, io, led) is libam type 2..=5.
                    kind: target.kind as u32 + 1,
                    id: target.id,
                    product: wide("can", &target.product)?,
                    ..am::CanTarget::default()
                },
                interval: u64::from(reply.interval),
                input_mask: mask,
                coin_input: u64::from(reply.coin_slot),
                auto_reply: reply.coin_share_slot,
            };
            append("can", &mut board.nodes, node)?;
        }
        append("can", &mut out.can.ports, board)?;
    }
    Ok(())
}

/// 8: `UsbIoConverter` (`0x140012110`, `spec/libam/usbio.md` section 4).
fn usb_io(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.usb_io;
    out.usbio.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    for node in &section.nodes {
        let function = &node.required_function;
        let mut funcs = am::UsbIoFuncs {
            switch_input: function.switch_input.map(|s| Pair { a: u64::from(s.player), b: u64::from(s.button) }),
            coin_input: function.coin_input.map(u64::from),
            analog_input: function.analog_input.map(|a| Pair { a: u64::from(a.channel), b: u64::from(a.bit) }),
            rotary_input: function.rotary_input.map(u64::from),
            general_output: function.general_output.map(u64::from),
            pwm_output: function.pwm_output.map(u64::from),
            ..am::UsbIoFuncs::default()
        };
        for unique in &function.unique_io {
            // Exactly three parameter bytes are pushed whatever the JSON gave; `input` / `output` stay behind.
            let mut command = UniqueCmd::new(unique.command);
            for param in unique.params {
                command.push_param(param).map_err(|error| fail("usb_io", error))?;
            }
            append("usb_io", &mut funcs.unique_io, command)?;
        }
        let converted = am::UsbIoNode {
            id: node.id,
            share_coin_slot: node.share_coin_slot,
            auto_output_zero: node.auto_output_zero,
            comm_timeout: node.timeout,
            switch_sampling: node.switch_sampling,
            // Always engaged: 1 when the key was not read.
            pwm_cycle: Some(node.pwm_cycle),
            funcs,
        };
        append("usb_io", &mut out.usbio.nodes, converted)?;
    }
    out.usbio.firmware_path = wide("usb_io", &section.firmware_path)?;
    out.usbio.append_sub_number_to_device_error = section.append_sub_number_to_device_error;
    Ok(())
}

/// 9: `CreditConverter` (`0x140010540`).
fn credit(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.credit;
    let credit = &mut out.credit;
    credit.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    credit.max_credit = section.max_credit;
    for (slot, &assign) in credit.coin_box_assign.iter_mut().zip(&section.coin_box_assign) {
        *slot = if assign == UNSET_U32 { UNSET_U64 } else { u64::from(assign) };
    }

    // A `-1` stays "unset".
    let given = |value: i32| u32::try_from(value).ok();
    let rule = &mut credit.rule;
    // "common" is 1 and "individual" 2; the free-play flag is 2 when set and 1 when clear.
    rule.chute_share_type = section.coin_chute_type_common.map_or(0, |common| if common { 1 } else { 2 });
    rule.service_share_type = section.service_type_common.map_or(0, |common| if common { 1 } else { 2 });
    rule.operation = section.freeplay.map_or(0, |free| if free { 2 } else { 1 });
    for (slot, &value) in rule.coin_chute_multiplier.iter_mut().zip(&section.coin_chute_multiplier) {
        if let Some(value) = given(value) {
            *slot = value;
        }
    }
    if let Some(value) = given(section.coin_to_credit) {
        rule.coin_to_credit = value;
    }
    if let Some(value) = given(section.bonus_adder) {
        rule.bonus_adder = value;
    }
    for (slot, &value) in rule.game_cost.iter_mut().zip(&section.game_cost) {
        if let Some(value) = given(value) {
            *slot = value;
        }
    }

    let selector = &section.as6db;
    if selector.enable {
        credit.coin_selector_as6db.enable = true;
        credit.coin_selector_as6db.arg = u64::from(selector.port);
        for &code in &selector.counted_coin {
            let code = if (1..=3).contains(&code) { code } else { 0 };
            append("credit", &mut credit.coin_selector_as6db.types, code)?;
        }
    }
    Ok(())
}

/// 10: UsbDevice converter (`0x140011D30`).
fn usb_device(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    for requirement in &src.config.usb_device.required {
        let mut required = UsbRequired::default();
        for &(vendor_id, product_id) in &requirement.ids {
            required.add_pair(vendor_id, product_id).map_err(|error| fail("usb_device", error))?;
        }
        required.set_count(requirement.count);
        out.usbdevice.app.add_required(required).map_err(|error| fail("usb_device", error))?;
    }
    Ok(())
}

/// 11: `NetworkConverter` (`0x140011930`). Never fails.
fn network(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.network;
    let network = &mut out.network;
    network.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    network.support_standalone = section.support_standalone;
    network.use_gateway = section.use_gateway;
    network.use_location_router = section.use_location_router;
    // deny 0, require 1, allow 2.
    network.location_router_directly = section.location_router_directly as u32;
    if let Some(property) = section.property {
        network.property = Some(crate::network::NetworkProperty {
            dhcp: property.dhcp,
            address: property.ip_address,
            subnet_mask: property.subnet_mask,
            gateway: property.gateway,
            primary_dns: property.primary_dns,
            secondary_dns: property.secondary_dns,
        });
        // The byte libam keeps is the negation of `save`.
        network.dhcp_if_unset = !property.save;
    }
    Ok(())
}

/// 12: `LanInstallConverter` (`0x1400116C0`). Never fails; the interval is written even when disabled.
fn lan_install(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.lan_install;
    // Client 1, server 2.
    out.laninstall.mode = if section.enable { u32::from(section.server) + 1 } else { 0 };
    out.laninstall.interval = u64::from(section.interval);
    Ok(())
}

/// 13: `NetDeliveryConverter` (`0x1400117F0`). Never fails; always fully written.
fn net_delivery(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.net_delivery;
    out.netdeliver = am::NetDeliverConfig {
        enable: section.enable,
        local: section.local,
        const_one: true,
        use_aws_signature: section.use_aws_signature,
    };
    Ok(())
}

/// The `AllnetVersion` builder `0x14021F550` for a `<major>.<minor>` text.
fn allnet_version(text: &str) -> Result<AllnetVersion, ConvertError> {
    let parsed =
        text.split_once('.').and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)));
    let Some((major, minor)) = parsed else {
        return Err(fail("allnet_auth", format!("invalid develop version \"{text}\"")));
    };
    if major > 9999 || minor > 99 {
        return Err(fail("allnet_auth", format!("develop version {text} is out of range")));
    }
    let packed = (major << 16) | (minor << 8);
    let (long_text, short_text) = if packed == 0 {
        ("--.--.--".to_owned(), "--.--".to_owned())
    } else {
        (format!("{major}.{minor:02}.00"), format!("{major}.{minor:02}"))
    };
    Ok(AllnetVersion { valid: true, packed, long_text, short_text })
}

/// 14: Auth converter (`0x140012F40`). The log level is the max-merged byte the callback computed.
fn auth(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.auth;
    let auth = &mut out.allnet_auth;
    auth.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    // Bit 0 (value 1) is never set.
    auth.support_line =
        [(section.support_line.broadband, 2), (section.support_line.mobile, 4), (section.support_line.xdsl, 8)]
            .into_iter()
            .filter(|&(set, _)| set)
            .fold(0, |bits, (_, bit)| bits | bit);
    // Two libam fields carry the original's offsets with different names: the byte at `+8` is the JSON
    // `multi_auth_method` (libam reads it as its develop-version flag), and the u32 at `+0x3C` is the merged
    // log level mapped 1..=3 -> same, 0 and 4 -> 0 (libam's `multi_auth_method`).
    auth.develop_version = section.multi_auth_method;
    auth.multi_auth_method = match src.auth_log_level {
        level @ 1..=3 => u32::from(level),
        _ => 0,
    };
    if !section.develop_version.is_empty() {
        auth.version = allnet_version(&section.develop_version)?;
    }
    auth.lite_auth_key = section.lite_auth_key;
    auth.auth_type = section.auth_type as u32;
    Ok(())
}

/// 15: Accounting converter (`0x140012D90`): only the mode reaches libam.
fn accounting(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.accounting;
    out.allnet_accounting.mode = if section.enable { section.mode as u32 } else { 0 };
    Ok(())
}

/// 16: WiFi converter (`0x140013290`): only the enable flag reaches libam.
fn wifi(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    out.allnet_wifi.enable = src.config.wifi.enable;
    Ok(())
}

/// 17: Aime converter (`0x14000F2E0`).
fn aime(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.aime;
    let aime = &mut out.aime;
    aime.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    for unit in &section.units {
        let converted =
            am::AimeUnit { com_port: u64::from(unit.port), unit_index: u64::from(unit.id), flag: unit.required };
        append("aime", &mut aime.units, converted)?;
    }
    // Empty entries are skipped.
    for path in section.firmware_path.iter().filter(|path| !path.is_empty()) {
        append("aime", &mut aime.firmware_paths, wide("aime", path)?)?;
    }
    aime.high_baudrate = section.high_baudrate;
    aime.polling_interval = section.polling_interval;
    // Written as a constant, whatever the file says.
    aime.card_accept_time = 2_147_483;
    aime.log_enable = section.log_enable;
    aime.log_root_path = wide("aime", &terminate_path(&section.log_root_path))?;
    Ok(())
}

/// libam brand code by `emoney.ignore_brand` index (`0x140052360`).
const EMONEY_BRAND_CODES: [usize; 7] = [1, 2, 4, 5, 6, 3, 7];

/// 18: EMoney converter (`0x140010C60`); receives the executable directory.
fn emoney(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.emoney;
    let emoney = &mut out.emoney;
    emoney.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    emoney.root_path = wide("emoney", &ensure_separator(src.exe_dir))?;
    emoney.resource_path = wide("emoney", &section.resource_path)?;
    emoney.log_level = u32::from(section.log_level);
    emoney.log_root_path = wide("emoney", &terminate_path(&section.log_root_path))?;
    emoney.aime_unit = u64::from(section.aime_unit);
    for (&ignored, code) in section.ignore_brand.iter().zip(EMONEY_BRAND_CODES) {
        if ignored {
            emoney.brand_enabled[code] = false;
        }
    }
    Ok(())
}

/// 19: AimePay converter (`0x14000F6F0`); `display_port` is the callback's business.
fn aime_pay(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.aimepay;
    let pay = &mut out.aimepay;
    pay.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    pay.resource_path = wide("aimepay", &section.resource_path)?;
    // Only non-empty ids are appended, so an empty first id moves the second one up.
    for id in section.coin_item_id.iter().filter(|id| !id.is_empty()) {
        append("aimepay", &mut pay.coin_item_id, wide("aimepay", id)?)?;
    }
    Ok(())
}

/// 20: `abaas_log` converter (`0x140012B40`).
fn abaas_log(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    let section = &src.config.abaas_log;
    let abaas = &mut out.abaas_log;
    abaas.enable = section.enable;
    if !section.enable {
        return Ok(());
    }
    // The setter needs fewer than 200 units; the stored value carries the trailing backslash.
    if section.root_path.encode_utf16().count() >= 200 {
        return Err(fail("abaas_log", "root path is too long"));
    }
    abaas.root_path = wide("abaas_log", &terminate_path(&section.root_path))?;
    abaas.use_network = section.use_network;
    abaas.level = match section.log_level {
        level @ 1..=3 => u32::from(level),
        _ => 0,
    };
    Ok(())
}

/// 21: Apm converter (`0x14000F9B0`).
fn apm(src: &Source<'_>, out: &mut AmConfig) -> Outcome {
    out.apm.enable = src.config.apm;
    Ok(())
}
