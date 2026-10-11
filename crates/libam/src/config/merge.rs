//! Merge of the selected config files into one JSON tree (`spec/libam/config.md` Part C §3-§6).
//!
//! Only the 24 section keys are looked at; every other top-level key (`condition`, typos) is dropped. Inside a
//! section every key is copied, known or not. The rules per value: an absent source key changes nothing; an
//! absent target key is created with the default of the source's JSON type and then merged; a type difference is
//! an error; scalars are replaced; objects merge per key in sorted order; arrays are appended or replaced
//! depending on the section.

use super::json::{Json, Map};

/// A merge failure: the message and the key path, root first (array indices as decimal strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MergeError {
    what: String,
    path: Vec<String>,
}

impl MergeError {
    fn new(what: impl Into<String>) -> Self {
        Self { what: what.into(), path: Vec::new() }
    }

    /// Prepends `key` to the path; an empty key is skipped.
    fn at(mut self, key: &str) -> Self {
        if !key.is_empty() {
            self.path.insert(0, key.to_owned());
        }
        self
    }

    /// `Config merge error (<file>) at "<path>". <what>`, or without the ` at "<path>"` part for an empty path.
    pub(super) fn render(&self, file: &str) -> String {
        let mut path = String::new();
        for elem in &self.path {
            // A fold step: both parts non-empty -> joined with `.` (or `[..]` for a digit start); otherwise the
            // element replaces the accumulator.
            path = if !path.is_empty() && !elem.is_empty() {
                if elem.starts_with(|c: char| c.is_ascii_digit()) {
                    format!("{path}[{elem}]")
                } else {
                    format!("{path}.{elem}")
                }
            } else {
                elem.clone()
            };
        }
        let mut text = format!("Config merge error ({file})");
        if !path.is_empty() {
            text.push_str(&format!(" at \"{path}\""));
        }
        text.push_str(&format!(". {}", self.what));
        text
    }
}

type MergeResult<T = ()> = Result<T, MergeError>;

/// How arrays below a section combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arrays {
    Append,
    Replace,
}

/// The merge strategy of one section.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Plain recursive merge (no `enable` rule).
    Plain(Arrays),
    /// Recursive merge preceded by the `enable` rule.
    Enabled(Arrays),
    Jvs,
    /// `input` / `output`: `system` and `players` get tagged.
    InOut,
    Credit,
}

/// The dispatcher order of the original (it is independent of the file content).
const SECTIONS: [(&str, Kind); 24] = [
    ("common", Kind::Plain(Arrays::Replace)),
    ("error", Kind::Plain(Arrays::Append)),
    ("sequence", Kind::Plain(Arrays::Replace)),
    ("video", Kind::Enabled(Arrays::Replace)),
    ("usb_device", Kind::Plain(Arrays::Append)),
    ("jvs", Kind::Jvs),
    ("can", Kind::Enabled(Arrays::Append)),
    ("usb_io", Kind::Enabled(Arrays::Append)),
    ("input", Kind::InOut),
    ("output", Kind::InOut),
    ("credit", Kind::Credit),
    ("backup", Kind::Plain(Arrays::Append)),
    ("network", Kind::Enabled(Arrays::Append)),
    ("lan_install", Kind::Enabled(Arrays::Append)),
    ("net_delivery", Kind::Enabled(Arrays::Append)),
    ("allnet_auth", Kind::Enabled(Arrays::Append)),
    ("allnet_accounting", Kind::Enabled(Arrays::Append)),
    ("allnet_wifi", Kind::Enabled(Arrays::Append)),
    ("allnet_air", Kind::Enabled(Arrays::Append)),
    ("aime", Kind::Enabled(Arrays::Replace)),
    ("abaas_log", Kind::Enabled(Arrays::Append)),
    ("emoney", Kind::Enabled(Arrays::Append)),
    ("aimepay", Kind::Enabled(Arrays::Append)),
    ("apm", Kind::Enabled(Arrays::Append)),
];

/// What the `input` / `output` mergers need: the file's `condition.jvs_node` and the tag key name.
struct Tagging<'a> {
    jvs_node: u32,
    key: &'a str,
}

/// Merges one parsed file (`root`) into `tree`.
///
/// `jvs_node` is the file's `condition.jvs_node`; `tag_key` is the name of the synthetic key added to every
/// appended `input` / `output` device object.
pub(super) fn merge_file(tree: &mut Map, root: &Json, jvs_node: u32, tag_key: &str) -> MergeResult {
    let Json::Object(src) = root else {
        return Err(MergeError::new("JSON root is not object."));
    };
    let tagging = Tagging { jvs_node, key: tag_key };
    for (name, kind) in SECTIONS {
        with_key(src, tree, name, |s, d| {
            same_type(s, d)?;
            section(kind, s, d, &tagging)
        })?;
    }
    Ok(())
}

/// The per-key wrapper: absent source key -> nothing; absent target key -> a default of the source type; errors
/// get `key` prepended to their path.
fn with_key(src: &Map, dst: &mut Map, key: &str, f: impl FnOnce(&Json, &mut Json) -> MergeResult) -> MergeResult {
    let Some(s) = src.get(key) else { return Ok(()) };
    let d = dst.entry(key.to_owned()).or_insert_with(|| s.empty_like());
    f(s, d).map_err(|e| e.at(key))
}

fn same_type(s: &Json, d: &Json) -> MergeResult {
    if s.type_name() == d.type_name() {
        Ok(())
    } else {
        Err(MergeError::new(format!(
            "source type ({}) and target type ({}) are not the same.",
            s.type_name(),
            d.type_name()
        )))
    }
}

/// The worker type guards: source and target must both be objects.
fn objects<'a>(s: &'a Json, d: &'a mut Json) -> MergeResult<(&'a Map, &'a mut Map)> {
    match (s, d) {
        (Json::Object(sm), Json::Object(dm)) => Ok((sm, dm)),
        (Json::Object(_), _) => Err(MergeError::new("target type is invalid.")),
        _ => Err(MergeError::new("source type is invalid.")),
    }
}

/// The array guard of the `node` / `system` / `players` workers.
fn arrays<'a>(s: &'a Json, d: &'a mut Json) -> MergeResult<(&'a [Json], &'a mut Vec<Json>)> {
    match (s, d) {
        (Json::Array(sa), Json::Array(da)) => Ok((sa, da)),
        (Json::Array(_), _) => Err(MergeError::new("target type is invalid.")),
        _ => Err(MergeError::new("source type is invalid.")),
    }
}

fn section(kind: Kind, s: &Json, d: &mut Json, tagging: &Tagging<'_>) -> MergeResult {
    let (sm, dm) = objects(s, d)?;
    match kind {
        Kind::Plain(arrays) => children(sm, dm, arrays),
        Kind::Enabled(arrays) => {
            if enable_rule(sm, dm)? {
                children(sm, dm, arrays)?;
            }
            Ok(())
        }
        Kind::Jvs => {
            if !enable_rule(sm, dm)? {
                return Ok(());
            }
            for key in sm.keys() {
                if key == "node" {
                    with_key(sm, dm, key, |s, d| {
                        same_type(s, d)?;
                        jvs_nodes(s, d, tagging.jvs_node)
                    })?;
                } else {
                    child(sm, dm, key, Arrays::Append)?;
                }
            }
            Ok(())
        }
        Kind::InOut => {
            for key in sm.keys() {
                match key.as_str() {
                    "system" => with_key(sm, dm, key, |s, d| {
                        same_type(s, d)?;
                        let (sa, da) = arrays(s, d)?;
                        append_tagged(sa, da, tagging)
                    })?,
                    "players" => with_key(sm, dm, key, |s, d| {
                        same_type(s, d)?;
                        players(s, d, tagging)
                    })?,
                    _ => child(sm, dm, key, Arrays::Append)?,
                }
            }
            Ok(())
        }
        Kind::Credit => {
            if !enable_rule(sm, dm)? {
                return Ok(());
            }
            for key in sm.keys() {
                if key == "coin_selector_AS6DB" {
                    with_key(sm, dm, key, |s, d| {
                        same_type(s, d)?;
                        let (sm, dm) = objects(s, d)?;
                        if enable_rule(sm, dm)? {
                            children(sm, dm, Arrays::Replace)?;
                        }
                        Ok(())
                    })?;
                } else {
                    child(sm, dm, key, Arrays::Replace)?;
                }
            }
            Ok(())
        }
    }
}

/// The generic child merge: object -> recursive, array -> append or replace, anything else -> assign.
fn child(src: &Map, dst: &mut Map, key: &str, arrays: Arrays) -> MergeResult {
    with_key(src, dst, key, |s, d| {
        same_type(s, d)?;
        match (s, d) {
            (Json::Object(sm), Json::Object(dm)) => children(sm, dm, arrays),
            (Json::Array(sa), Json::Array(da)) => {
                match arrays {
                    Arrays::Append => da.extend(sa.iter().cloned()),
                    Arrays::Replace => da.clone_from(sa),
                }
                Ok(())
            }
            (s, d) => {
                d.clone_from(s);
                Ok(())
            }
        }
    })
}

/// The child loop: every source key in sorted order.
fn children(src: &Map, dst: &mut Map, arrays: Arrays) -> MergeResult {
    for key in src.keys() {
        child(src, dst, key, arrays)?;
    }
    Ok(())
}

/// The `enable` rule: returns whether the rest of the source section is merged.
fn enable_rule(src: &Map, dst: &mut Map) -> MergeResult<bool> {
    let Some(source_enable) = src.get("enable") else {
        match dst.get("enable") {
            Some(Json::Bool(_)) | None => {}
            Some(_) => return Err(MergeError::new("target type is not boolean.").at("enable")),
        }
        dst.insert("enable".to_owned(), Json::Bool(true));
        return Ok(true);
    };
    let Json::Bool(value) = source_enable else {
        return Err(MergeError::new("source type is not boolean.").at("enable"));
    };
    with_key(src, dst, "enable", |s, d| {
        same_type(s, d)?;
        d.clone_from(s);
        Ok(())
    })?;
    Ok(*value)
}

/// `jvs.node`: an array of groups, each group an array of 1..=4 node configurations whose length is the node
/// count it applies to.
fn jvs_nodes(s: &Json, d: &mut Json, jvs_node: u32) -> MergeResult {
    let (source, target) = arrays(s, d)?;
    for (i, group) in target.iter().enumerate() {
        check_group(group, "target").map_err(|e| e.at(&i.to_string()))?;
    }
    // Groups that existed before this file; groups pushed by this file are never matched again.
    let existing = target.len();
    let mut place = |group: &[Json]| {
        let mut matched = false;
        for slot in target.iter_mut().take(existing) {
            if let Json::Array(inner) = slot
                && inner.len() == group.len()
            {
                inner.clone_from_slice(group);
                matched = true;
            }
        }
        if !matched {
            target.push(Json::Array(group.to_vec()));
        }
    };
    if jvs_node != 0 {
        if source.len() != jvs_node as usize {
            return Err(MergeError::new(format!(
                "source array size is not equals to \"condition.jvs_node\" ({jvs_node})."
            )));
        }
        place(source);
    } else {
        for (i, group) in source.iter().enumerate() {
            check_group(group, "source").map_err(|e| e.at(&i.to_string()))?;
            if let Json::Array(inner) = group {
                place(inner);
            }
        }
    }
    Ok(())
}

fn check_group(group: &Json, label: &str) -> MergeResult {
    let Json::Array(inner) = group else {
        return Err(MergeError::new(format!("{label} type is not array.")));
    };
    if inner.is_empty() {
        return Err(MergeError::new(format!("{label} array is empty.")));
    }
    if inner.len() > 4 {
        return Err(MergeError::new(format!("{label} array has too many node configs.")));
    }
    Ok(())
}

/// Appends `source` to `target` and tags every appended device object with the file's `jvs_node`.
fn append_tagged(source: &[Json], target: &mut Vec<Json>, tagging: &Tagging<'_>) -> MergeResult {
    let old = target.len();
    target.extend(source.iter().cloned());
    for (i, element) in target[old..].iter_mut().enumerate() {
        let Json::Object(object) = element else {
            return Err(MergeError::new("source type is not object.").at(&i.to_string()));
        };
        object.insert(tagging.key.to_owned(), Json::Number(f64::from(tagging.jvs_node)));
    }
    Ok(())
}

/// `players`: an array of per-player arrays; the target grows to the source length and never shrinks.
fn players(s: &Json, d: &mut Json, tagging: &Tagging<'_>) -> MergeResult {
    let (source, target) = arrays(s, d)?;
    while target.len() < source.len() {
        target.push(Json::Array(Vec::new()));
    }
    for (i, (s, d)) in source.iter().zip(target.iter_mut()).enumerate() {
        player(s, d, tagging).map_err(|e| e.at(&i.to_string()))?;
    }
    Ok(())
}

/// One player's list of `players`.
fn player(s: &Json, d: &mut Json, tagging: &Tagging<'_>) -> MergeResult {
    let Json::Array(sa) = s else {
        return Err(MergeError::new("source type is not array."));
    };
    let Json::Array(da) = d else {
        return Err(MergeError::new("target type is not array."));
    };
    append_tagged(sa, da, tagging)
}
