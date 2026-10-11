//! The six cross-section validators, the prelude and the epilogue that run after every section validator
//! (`spec/libam/config.md` Part E §3, Part H §4, Part I §5, Part J §4-§6; driver `0x14008DC90`).
//!
//! The order is IO, Network, Install, Allnet, EMoney/AimePay, ComPort; the first failure stops the chain. Every
//! failure is a complete message (the validators do not wrap it in `Config error at`, except where the original's
//! text already starts with it).

use std::collections::HashMap;

use super::Config;
use super::allnet::AuthType;
use super::network::LocationRouterDirectly;

/// The message of a failed cross-section rule.
pub(super) type MixedResult = Result<(), String>;

fn inconsistent(a: &str, b: &str) -> String {
    format!("\"{a}\" and \"{b}\" are inconsistent.")
}

/// Runs the six validators in order, then the epilogue.
pub(super) fn validate(cfg: &mut Config) -> MixedResult {
    io(cfg)?;
    network(cfg)?;
    install(cfg)?;
    allnet(cfg)?;
    emoney_aimepay(cfg)?;
    com_port(cfg)?;
    epilogue(cfg);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// IO: which board provides the coin input, and the AS6DB coin selector against it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoinBoard {
    None,
    Jvs,
    Can,
    Usb,
}

/// One board's coin-input hit: the message fragment.
struct CoinScan {
    jvs: Option<String>,
    can: Option<String>,
    usb: Option<String>,
}

fn scan_coin_inputs(cfg: &Config) -> CoinScan {
    let mut scan = CoinScan { jvs: None, can: None, usb: None };
    if cfg.jvs.enable {
        // The first hit of the last slot that has any.
        for (i, list) in cfg.jvs.lists.iter().enumerate() {
            if let Some(j) = list.iter().position(|node| node.required_function.coin_input.is_some()) {
                scan.jvs = Some(format!("jvs.node[{i}][{j}].required_function.coin_input"));
            }
        }
    }
    if cfg.can.enable {
        // The first hit of the last port that has any.
        for (i, port) in cfg.can.ports.iter().enumerate() {
            let hit = port.targets.iter().position(|t| t.auto_reply.interval != 0 && t.auto_reply.coin_slot != 0);
            if let Some(j) = hit {
                scan.can = Some(format!("can.port[{i}].target[{j}].auto_reply.coin_input"));
            }
        }
    }
    if cfg.usb_io.enable {
        let hit = cfg.usb_io.nodes.iter().position(|n| n.required_function.coin_input.is_some());
        if let Some(k) = hit {
            scan.usb = Some(format!("usb_io.node[{k}].required_function.coin_input"));
        }
    }
    scan
}

fn coin_input_board(cfg: &Config) -> Result<CoinBoard, String> {
    let scan = scan_coin_inputs(cfg);
    let both = |a: &str, b: &str| format!("Both \"{a}\" and \"{b}\" must not be enabled.");
    match (&scan.jvs, &scan.can, &scan.usb) {
        (Some(j), Some(c), _) => Err(both(j, c)),
        (Some(j), None, Some(u)) => Err(both(j, u)),
        (None, Some(c), Some(u)) => Err(both(c, u)),
        (Some(_), None, None) => Ok(CoinBoard::Jvs),
        (None, Some(_), None) => Ok(CoinBoard::Can),
        (None, None, Some(_)) => Ok(CoinBoard::Usb),
        (None, None, None) => Ok(CoinBoard::None),
    }
}

const AS6DB: &str = "Config error at \"credit.coin_selector_AS6DB\".";
const AS6DB_COUNTED: &str = "Config error at \"credit.coin_selector_AS6DB.counted_coin\".";

fn io(cfg: &Config) -> MixedResult {
    let board = coin_input_board(cfg)?;
    let selector = &cfg.credit.as6db;
    if !cfg.credit.enable || !selector.enable {
        return Ok(());
    }
    let count = selector.counted_coin.len() as u32;
    match board {
        CoinBoard::None => Err(format!("{AS6DB} Coin input function is not enabled in all of the I/O board config.")),
        CoinBoard::Jvs => {
            for list in cfg.jvs.lists.iter().filter(|list| !list.is_empty()) {
                let Some(slots) = list.iter().rev().find_map(|node| node.required_function.coin_input) else {
                    return Err(format!("{AS6DB} Coin input function of JVS nodes is disabled."));
                };
                if count != slots {
                    return Err(format!(
                        "{AS6DB_COUNTED} Array size is not match the coin slot count of the JVS node."
                    ));
                }
            }
            Ok(())
        }
        CoinBoard::Can => {
            let slot = cfg
                .can
                .ports
                .iter()
                .flat_map(|port| &port.targets)
                .find(|t| t.auto_reply.interval != 0 && t.auto_reply.coin_slot != 0)
                .map(|t| u32::from(t.auto_reply.coin_slot));
            match slot {
                None => Err(format!("{AS6DB} Coin input function of CAN targets is disabled.")),
                Some(slot) if slot == count => Ok(()),
                Some(_) => {
                    Err(format!("{AS6DB_COUNTED} Array size is not match the coin slot count of the CAN target."))
                }
            }
        }
        CoinBoard::Usb => {
            let slot = cfg.usb_io.nodes.iter().find_map(|n| n.required_function.coin_input);
            match slot {
                None => Err(format!("{AS6DB} Coin input function of USB nodes is disabled.")),
                Some(slot) if slot == count => Ok(()),
                Some(_) => Err(format!("{AS6DB_COUNTED} Array size is not match the coin slot count of the USB node.")),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Network, install.

fn network(cfg: &Config) -> MixedResult {
    let n = &cfg.network;
    let l = &cfg.lan_install;
    let d = &cfg.net_delivery;
    if n.enable {
        if !n.use_gateway {
            if cfg.auth.enable {
                return Err(inconsistent("network.use_gateway", "allnet_auth.enable"));
            }
            if cfg.air {
                return Err(inconsistent("network.use_gateway", "allnet_air.enable"));
            }
            if l.enable {
                return Err(inconsistent("network.use_gateway", "lan_install.enable"));
            }
            if d.enable && d.local {
                return Err(inconsistent("network.use_gateway", "net_delivery.local"));
            }
        }
        if n.location_router_directly == LocationRouterDirectly::Require {
            if !n.use_location_router {
                return Err(inconsistent("network.use_location_router", "network.location_router_directly"));
            }
            if l.enable {
                return Err(inconsistent("network.location_router_directly", "lan_install.enable"));
            }
            if d.enable && d.local {
                return Err(inconsistent("network.location_router_directly", "net_delivery.local"));
            }
        }
        return Ok(());
    }
    let first_enabled = [
        ("lan_install", l.enable),
        ("net_delivery", d.enable),
        ("allnet_auth", cfg.auth.enable),
        ("allnet_accounting", cfg.accounting.enable),
        ("allnet_wifi", cfg.wifi.enable),
        ("allnet_air", cfg.air),
        ("emoney", cfg.emoney.enable),
        ("aimepay", cfg.aimepay.enable),
    ]
    .into_iter()
    .find_map(|(name, enabled)| enabled.then_some(name));
    match first_enabled {
        Some(name) => Err(inconsistent("network.enable", &format!("{name}.enable"))),
        None => Ok(()),
    }
}

fn install(cfg: &Config) -> MixedResult {
    let l = &cfg.lan_install;
    let d = &cfg.net_delivery;
    if !l.enable || !d.enable {
        return Ok(());
    }
    if !l.server {
        return Err(inconsistent("lan_install.server", "net_delivery.enable"));
    }
    if d.local {
        return Err(inconsistent("lan_install.enable", "net_delivery.local"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Allnet.

fn allnet(cfg: &Config) -> MixedResult {
    let auth = &cfg.auth;
    let d = &cfg.net_delivery;
    if auth.enable {
        if !auth.support_line.any() {
            return Err("A value of true is not contained in \"allnet_auth.support_line\".".to_owned());
        }
        if auth.auth_type == AuthType::V1 && d.enable && d.use_aws_signature {
            return Err(inconsistent("allnet_auth.type", "net_delivery.use_aws_signature"));
        }
    } else {
        let name = if d.enable && !d.local {
            Some("net_delivery")
        } else if cfg.accounting.enable {
            Some("allnet_accounting")
        } else if cfg.wifi.enable {
            Some("allnet_wifi")
        } else if cfg.aimepay.enable {
            Some("aimepay")
        } else {
            None
        };
        if let Some(name) = name {
            return Err(inconsistent("allnet_auth.enable", &format!("{name}.enable")));
        }
    }

    // A disabled section keeps level 0 ("unspecified"), which never conflicts.
    let levels = [
        ("allnet_auth", if auth.enable { auth.log_level } else { 0 }),
        ("allnet_accounting", if cfg.accounting.enable { cfg.accounting.log_level } else { 0 }),
        ("allnet_wifi", if cfg.wifi.enable { cfg.wifi.log_level } else { 0 }),
    ];
    for (a, b) in [(0, 1), (0, 2), (1, 2)] {
        let ((name_a, level_a), (name_b, level_b)) = (levels[a], levels[b]);
        if level_a != 0 && level_b != 0 && level_a != level_b {
            return Err(format!("\"{name_a}.log.level\" and \"{name_b}.log.level\" are different."));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// EMoney and AimePay.

/// `GetFullPathNameW` on a path that is already prefixed with the executable directory: separators unified,
/// repeated separators and `.` dropped, `..` resolved lexically. Case is kept (the comparison is exact).
fn normalize_path(path: &str) -> String {
    let rooted = path.starts_with(['\\', '/']);
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                // A drive prefix (`C:`) is never popped.
                if parts.last().is_some_and(|last| *last != ".." && !last.ends_with(':')) {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("\\");
    if rooted { format!("\\{joined}") } else { joined }
}

fn emoney_aimepay(cfg: &Config) -> MixedResult {
    let aime = &cfg.aime;
    let e = &cfg.emoney;
    let p = &cfg.aimepay;
    if e.enable {
        if !aime.enable {
            return Err(inconsistent("emoney.enable", "aime.enable"));
        }
        if usize::from(e.aime_unit) >= aime.units.len() {
            return Err("\"emoney.aime_unit\" is out of \"aime.unit\" range.".to_owned());
        }
        if !aime.high_baudrate {
            return Err(inconsistent("emoney.enable", "aime.high_baudrate"));
        }
    }
    if p.enable && !aime.enable {
        return Err(inconsistent("aimepay.enable", "aime.enable"));
    }
    match (e.enable, p.enable) {
        (false, false) => Ok(()),
        (false, true) if p.resource_path.is_empty() => Err("\"aimepay.resource_path\" is empty.".to_owned()),
        (true, false) if e.resource_path.is_empty() => Err("\"emoney.resource_path\" is empty.".to_owned()),
        (false, true) | (true, false) => Ok(()),
        (true, true) => match (e.resource_path.is_empty(), p.resource_path.is_empty()) {
            (true, true) => Err("Both \"emoney.resource_path\" and \"aimepay.resource_path\" are empty.".to_owned()),
            (true, false) | (false, true) => Ok(()),
            (false, false) => {
                if normalize_path(&e.resource_path) == normalize_path(&p.resource_path) {
                    Ok(())
                } else {
                    Err("\"emoney.resource_path\" and \"aimepay.resource_path\" are different.".to_owned())
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------------------------------------------
// COM ports.

fn com_port(cfg: &Config) -> MixedResult {
    // (port, name) in insertion order.
    let mut entries: Vec<(u32, String)> = Vec::new();
    if cfg.credit.enable && cfg.credit.as6db.enable {
        entries.push((u32::from(cfg.credit.as6db.port), "credit.coin_selector_AS6DB.port".to_owned()));
    }
    if cfg.aime.enable {
        for (i, unit) in cfg.aime.units.iter().enumerate() {
            entries.push((unit.port, format!("aime.unit[{i}].port")));
        }
    }

    let e = &cfg.emoney;
    let p = &cfg.aimepay;
    let (ep, ap) = (e.display_port, p.display_port);
    match (e.enable, p.enable) {
        (true, true) => {
            if ep == 0 && ap == 0 {
                return Err("Both \"emoney.display_port\" and \"aimepay.display_port\" are zero.".to_owned());
            }
            if ep != 0 && ap != 0 && ap != ep {
                return Err("\"emoney.display_port\" and \"aimepay.display_port\" are different.".to_owned());
            }
            if ep != 0 {
                entries.push((ep, "emoney.display_port".to_owned()));
            } else {
                entries.push((ap, "aimepay.display_port".to_owned()));
            }
        }
        (true, false) => {
            if ep == 0 {
                return Err("\"emoney.display_port\" is out of range.".to_owned());
            }
            entries.push((ep, "emoney.display_port".to_owned()));
        }
        (false, true) => {
            if ap == 0 {
                return Err("\"aimepay.display_port\" is out of range.".to_owned());
            }
            entries.push((ap, "aimepay.display_port".to_owned()));
        }
        (false, false) => {}
    }

    let mut counts: HashMap<u32, usize> = HashMap::new();
    for (port, _) in &entries {
        *counts.entry(*port).or_default() += 1;
    }
    // The first group (in order of first insertion) with at least two members.
    let Some(&(port, _)) = entries.iter().find(|(port, _)| counts[port] >= 2) else { return Ok(()) };
    let names: Vec<String> =
        entries.iter().filter(|(p, _)| *p == port).map(|(_, name)| format!("\"{name}\"")).collect();
    let (last, head) = names.split_last().expect("a duplicated port has at least two names");
    let list = if head.is_empty() { last.clone() } else { format!("{} and {last}", head.join(", ")) };
    Err(format!("{list} are duplicated (COM{port})."))
}

// ---------------------------------------------------------------------------------------------------------------
// Epilogue.

/// With both `emoney` and `aimepay` enabled, an empty `resource_path` and a zero `display_port` are copied from
/// the other section.
fn epilogue(cfg: &mut Config) {
    if !cfg.emoney.enable || !cfg.aimepay.enable {
        return;
    }
    if cfg.emoney.resource_path.is_empty() {
        cfg.emoney.resource_path.clone_from(&cfg.aimepay.resource_path);
    } else if cfg.aimepay.resource_path.is_empty() {
        cfg.aimepay.resource_path.clone_from(&cfg.emoney.resource_path);
    }
    if cfg.emoney.display_port == 0 {
        cfg.emoney.display_port = cfg.aimepay.display_port;
    } else if cfg.aimepay.display_port == 0 {
        cfg.aimepay.display_port = cfg.emoney.display_port;
    }
}
