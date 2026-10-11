//! Typed reading of a merged section: presence, JSON type, range and the `Config error at "<path>". <msg>`
//! message format (`spec/libam/config.md` Part D §1, Part E §5).
//!
//! The original reads every key through a three-layer helper (path wrapper, required-flag worker, presence/type
//! leaf). The observable rules are:
//!
//! * a key is *present* when it exists and is not JSON `null`; `null` and absent are the same;
//! * a present key of the wrong JSON type fails with `Invalid type. value = <v>`;
//! * a range or enum check that fails turns into `<what>. value = <v>` (a `.` is appended to `<what>` when it
//!   does not end in one);
//! * a missing required key fails with `This key is required.` (no `value =` part);
//! * `<v>` is the value as [`Json::describe`] prints it (containers print as `array` / `object`).
//!
//! Numbers are doubles. Callers range-test the double and truncate toward zero themselves (`as` casts).

use super::json::{Json, Map};

/// A validation failure; the text is the complete `Config error at "<path>". <msg>` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConfigError(pub String);

pub(super) type Res<T = ()> = Result<T, ConfigError>;

/// `Config error at "<path>". <msg>`; the message part is omitted when empty.
pub(super) fn error(path: &str, msg: &str) -> ConfigError {
    if msg.is_empty() {
        ConfigError(format!("Config error at \"{path}\"."))
    } else {
        ConfigError(format!("Config error at \"{path}\". {msg}"))
    }
}

/// A failed check: `<thrown>[.] value = <v>`.
pub(super) fn bad_value(path: &str, thrown: &str, value: &Json) -> ConfigError {
    bad_described(path, thrown, &value.describe())
}

/// [`bad_value`] with the value text already rendered.
pub(super) fn bad_described(path: &str, thrown: &str, value: &str) -> ConfigError {
    let dot = if thrown.is_empty() || thrown.ends_with('.') { "" } else { "." };
    error(path, &format!("{thrown}{dot} value = {value}"))
}

pub(super) fn invalid_type(path: &str, value: &Json) -> ConfigError {
    error(path, &format!("Invalid type. value = {}", value.describe()))
}

pub(super) fn required(path: &str) -> ConfigError {
    error(path, "This key is required.")
}

/// `<parent>.<key>`; the first element is bare and a key that starts with a digit is rendered `[key]`.
pub(super) fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else if key.starts_with(|c: char| c.is_ascii_digit()) {
        format!("{parent}[{key}]")
    } else {
        format!("{parent}.{key}")
    }
}

/// An object being read, with its dotted path.
#[derive(Debug, Clone)]
pub(super) struct Obj<'a> {
    map: &'a Map,
    path: String,
}

impl<'a> Obj<'a> {
    pub(super) fn new(map: &'a Map, path: impl Into<String>) -> Self {
        Self { map, path: path.into() }
    }

    /// The object stored under `section` of the merged tree; `null` and absent give `None`.
    pub(super) fn section(root: &'a Map, section: &str) -> Res<Option<Self>> {
        match root.get(section) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Object(map)) => Ok(Some(Self::new(map, section))),
            Some(other) => Err(invalid_type(section, other)),
        }
    }

    pub(super) fn path(&self) -> &str {
        &self.path
    }

    pub(super) fn key_path(&self, key: &str) -> String {
        join(&self.path, key)
    }

    /// The error for a failed check of `key`.
    pub(super) fn bad(&self, key: &str, thrown: &str, value: &Json) -> ConfigError {
        bad_value(&self.key_path(key), thrown, value)
    }

    /// The value of `key` when present (exists and not `null`).
    pub(super) fn get(&self, key: &str) -> Option<&'a Json> {
        match self.map.get(key) {
            None | Some(Json::Null) => None,
            Some(v) => Some(v),
        }
    }

    pub(super) fn required_error(&self, key: &str) -> ConfigError {
        required(&self.key_path(key))
    }

    pub(super) fn bool(&self, key: &str) -> Res<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Json::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(invalid_type(&self.key_path(key), other)),
        }
    }

    pub(super) fn string(&self, key: &str) -> Res<Option<&'a str>> {
        match self.get(key) {
            None => Ok(None),
            Some(Json::String(s)) => Ok(Some(s)),
            Some(other) => Err(invalid_type(&self.key_path(key), other)),
        }
    }

    pub(super) fn number(&self, key: &str) -> Res<Option<f64>> {
        match self.get(key) {
            None => Ok(None),
            Some(Json::Number(n)) => Ok(Some(*n)),
            Some(other) => Err(invalid_type(&self.key_path(key), other)),
        }
    }

    /// A number that must satisfy `lo <= v <= hi`, else `thrown`; yields the double.
    pub(super) fn number_in(&self, key: &str, lo: f64, hi: f64, thrown: &str) -> Res<Option<f64>> {
        match self.number(key)? {
            Some(v) if v < lo || v > hi => Err(self.bad(key, thrown, &Json::Number(v))),
            other => Ok(other),
        }
    }

    /// A number checked by `check`, which names the failure (`Err(thrown)`) or maps the double to the stored value.
    pub(super) fn number_with<T>(
        &self,
        key: &str,
        check: impl FnOnce(f64) -> Result<T, &'static str>,
    ) -> Res<Option<T>> {
        match self.number(key)? {
            None => Ok(None),
            Some(v) => check(v).map(Some).map_err(|thrown| self.bad(key, thrown, &Json::Number(v))),
        }
    }

    pub(super) fn object(&self, key: &str) -> Res<Option<Obj<'a>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Json::Object(map)) => Ok(Some(Obj::new(map, self.key_path(key)))),
            Some(other) => Err(invalid_type(&self.key_path(key), other)),
        }
    }

    pub(super) fn array(&self, key: &str) -> Res<Option<Arr<'a>>> {
        match self.get(key) {
            None => Ok(None),
            Some(Json::Array(items)) => {
                Ok(Some(Arr { items, value: self.get(key).unwrap_or(&Json::Null), path: self.key_path(key) }))
            }
            Some(other) => Err(invalid_type(&self.key_path(key), other)),
        }
    }

    /// `object(key)` that must be present.
    pub(super) fn require_object(&self, key: &str) -> Res<Obj<'a>> {
        self.object(key)?.ok_or_else(|| self.required_error(key))
    }

    /// `array(key)` that must be present.
    pub(super) fn require_array(&self, key: &str) -> Res<Arr<'a>> {
        self.array(key)?.ok_or_else(|| self.required_error(key))
    }

    pub(super) fn require_number_with<T>(
        &self,
        key: &str,
        check: impl FnOnce(f64) -> Result<T, &'static str>,
    ) -> Res<T> {
        self.number_with(key, check)?.ok_or_else(|| self.required_error(key))
    }

    pub(super) fn require_string(&self, key: &str) -> Res<&'a str> {
        self.string(key)?.ok_or_else(|| self.required_error(key))
    }

    pub(super) fn require_bool(&self, key: &str) -> Res<bool> {
        self.bool(key)?.ok_or_else(|| self.required_error(key))
    }

    /// The shared `enable` reader of the `enable`-aware sections: `true` when the section object exists but has
    /// no `enable` key.
    pub(super) fn enable(&self) -> Res<bool> {
        Ok(self.bool("enable")?.unwrap_or(true))
    }

    /// An optional function sub-object: absent gives `None`; present with `enable: false` skips every other key
    /// (they are not even type-checked).
    pub(super) fn function<T>(&self, key: &str, body: impl FnOnce(&Obj<'a>) -> Res<T>) -> Res<Option<T>> {
        let Some(object) = self.object(key)? else { return Ok(None) };
        if !object.enable()? {
            return Ok(None);
        }
        body(&object).map(Some)
    }

    /// The `player` (`0..=max_player`) and `button` (`0..=64`) of a `switch_input` function; a non-zero button
    /// needs a non-zero player.
    pub(super) fn switch_pair(&self, max_player: f64) -> Res<(u32, u32)> {
        let player = self.number_with("player", range_u32(0.0, max_player))?.unwrap_or(0);
        let button = self
            .number_with("button", |v| {
                if !(0.0..=64.0).contains(&v) {
                    Err("out of range")
                } else if player == 0 && v as u32 != 0 {
                    Err("Non-zero value is invalid. Because \"player\" value is zero.")
                } else {
                    Ok(v as u32)
                }
            })?
            .unwrap_or(0);
        Ok((player, button))
    }
}

/// An array being read.
#[derive(Debug, Clone)]
pub(super) struct Arr<'a> {
    items: &'a [Json],
    value: &'a Json,
    path: String,
}

impl<'a> Arr<'a> {
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    /// A failed array-level check, reported at the array's own path (`value = array`).
    pub(super) fn bad(&self, thrown: &str) -> ConfigError {
        bad_value(&self.path, thrown, self.value)
    }

    /// A failed element check whose path still ends with the element index.
    pub(super) fn bad_at(&self, index: usize, thrown: &str) -> ConfigError {
        bad_value(&format!("{}[{index}]", self.path), thrown, self.value)
    }

    pub(super) fn element_path(&self, index: usize) -> String {
        format!("{}[{index}]", self.path)
    }

    /// Element `index` as an object (`Invalid type` at `<path>[i]` otherwise).
    pub(super) fn object(&self, index: usize) -> Res<Obj<'a>> {
        match &self.items[index] {
            Json::Object(map) => Ok(Obj::new(map, self.element_path(index))),
            other => Err(invalid_type(&self.element_path(index), other)),
        }
    }

    /// Element `index` as a number (`Invalid type` at `<path>[i]` otherwise).
    pub(super) fn number(&self, index: usize) -> Res<f64> {
        match &self.items[index] {
            Json::Number(n) => Ok(*n),
            other => Err(invalid_type(&self.element_path(index), other)),
        }
    }

    /// Element `index` as a string (`Invalid type` at `<path>[i]` otherwise).
    pub(super) fn string(&self, index: usize) -> Res<&'a str> {
        match &self.items[index] {
            Json::String(s) => Ok(s),
            other => Err(invalid_type(&self.element_path(index), other)),
        }
    }

    /// Element `index` as an array (`Invalid type` at `<path>[i]` otherwise).
    pub(super) fn array(&self, index: usize) -> Res<Arr<'a>> {
        match &self.items[index] {
            Json::Array(items) => Ok(Arr { items, value: &self.items[index], path: self.element_path(index) }),
            other => Err(invalid_type(&self.element_path(index), other)),
        }
    }
}

/// UTF-16 length of `s`, the unit of the original's fixed `wchar_t` buffers.
pub(super) fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// A check for [`Obj::number_with`]: `lo <= v <= hi`, stored as the truncating cast.
pub(super) fn range_u32(lo: f64, hi: f64) -> impl Fn(f64) -> Result<u32, &'static str> {
    move |v| if (lo..=hi).contains(&v) { Ok(v as u32) } else { Err("out of range") }
}
