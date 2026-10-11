//! The JSON tree the loader works on, and the byte-exact pretty printer of the `-m` dump.
//!
//! The original links picojson: numbers are `double`, objects are `std::map<string, value>` (unsigned byte-wise key
//! order), and the `-m` dump is its `serialize(prettify = true)` (`spec/libam/config.md` Part C §8). `serde_json`
//! parses the text; the tree itself is a small own type so that the key order and the `f64`-only numbers do not
//! depend on `serde_json` feature flags (`preserve_order` or `arbitrary_precision` may be enabled by a sibling
//! crate). The dump needs a hand-written serializer because picojson's output differs from `serde_json`'s: it
//! escapes `/` and 0x7F, renders numbers through `%.f` / `%.17g`, and ends with one newline.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// Object members in unsigned byte-wise key order, as `std::map<std::string, ...>` iterates them.
pub(super) type Map = BTreeMap<String, Json>;

/// One JSON value; every number is a double.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Map),
}

impl Json {
    /// The type name used by the merge error texts (`source type (<S>) and target type (<D>) ...`).
    pub(super) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
        }
    }

    /// A fresh value of the same JSON type with the type's default content (`null`, `false`, `0`, `""`, `[]`,
    /// `{}`): what the merge creates for a key that is missing in the target.
    pub(super) fn empty_like(&self) -> Self {
        match self {
            Self::Null => Self::Null,
            Self::Bool(_) => Self::Bool(false),
            Self::Number(_) => Self::Number(0.0),
            Self::String(_) => Self::String(String::new()),
            Self::Array(_) => Self::Array(Vec::new()),
            Self::Object(_) => Self::Object(Map::new()),
        }
    }

    /// Parses `text`; any JSON value is accepted as the root.
    pub(super) fn parse(text: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(text)
    }

    /// The text the validators put after `value = ` in their messages: numbers as in the dump, strings raw
    /// (no quotes), `true` / `false` / `null`, and the words `array` and `object` for containers.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => format_number(*n),
            Self::String(s) => s.clone(),
            Self::Array(_) => "array".to_owned(),
            Self::Object(_) => "object".to_owned(),
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Number(v as f64))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Number(v as f64))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
                Ok(Json::Number(v))
            }

            fn visit_str<E>(self, v: &str) -> Result<Json, E> {
                Ok(Json::String(v.to_owned()))
            }

            fn visit_string<E>(self, v: String) -> Result<Json, E> {
                Ok(Json::String(v))
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Json, A::Error> {
                let mut map = Map::new();
                while let Some((key, value)) = access.next_entry::<String, Json>()? {
                    // A duplicate key keeps the last value, as picojson's `operator[]` assignment does.
                    map.insert(key, value);
                }
                Ok(Json::Object(map))
            }
        }

        deserializer.deserialize_any(JsonVisitor).map_err(de::Error::custom)
    }
}

/// `2^53`: numbers below it with no fractional part print through `%.f`.
const TWO_POW_53: f64 = 9_007_199_254_740_992.0;

/// picojson's number rendering: `%.f` when `|x| < 2^53` and `x` is integral, else `%.17g` (with the three-digit
/// exponents of the Visual C++ 2012 runtime the original links: `1e22` prints `1e+022`).
pub(super) fn format_number(x: f64) -> String {
    if x.abs() < TWO_POW_53 && x.fract() == 0.0 {
        return format!("{x:.0}");
    }
    format_g17(x)
}

/// C `printf("%.17g", x)` for a finite `x`, with three exponent digits.
fn format_g17(x: f64) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".to_owned() } else { "0".to_owned() };
    }
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".to_owned()
        } else if x < 0.0 {
            "-inf".to_owned()
        } else {
            "inf".to_owned()
        };
    }
    // `{:.16e}` is `d.dddddddddddddddde<exp>`: 17 significant digits, correctly rounded.
    let rendered = format!("{x:.16e}");
    let (mantissa, exponent) = rendered.split_once('e').unwrap_or((&rendered, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();

    let mut out = String::from(sign);
    if !(-4..17).contains(&exponent) {
        let fraction = digits[1..].trim_end_matches('0');
        out.push_str(&digits[..1]);
        if !fraction.is_empty() {
            out.push('.');
            out.push_str(fraction);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:03}", exponent.unsigned_abs()));
    } else if exponent >= 0 {
        let int_len = exponent as usize + 1;
        let fraction = digits[int_len..].trim_end_matches('0');
        out.push_str(&digits[..int_len]);
        if !fraction.is_empty() {
            out.push('.');
            out.push_str(fraction);
        }
    } else {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(digits.trim_end_matches('0'));
    }
    out
}

/// The `-m` dump: picojson `serialize(prettify = true)` followed by one newline.
pub(super) fn dump_pretty(root: &Map) -> String {
    let mut out = String::new();
    write_object(&mut out, root, 1);
    out.push('\n');
    out
}

fn push_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_value(out: &mut String, value: &Json, depth: usize) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(n) => out.push_str(&format_number(*n)),
        Json::String(s) => write_string(out, s),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_indent(out, depth);
                write_value(out, item, depth + 1);
            }
            if !items.is_empty() {
                push_indent(out, depth - 1);
            }
            out.push(']');
        }
        Json::Object(map) => write_object(out, map, depth),
    }
}

fn write_object(out: &mut String, map: &Map, depth: usize) {
    out.push('{');
    for (i, (key, value)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_indent(out, depth);
        write_string(out, key);
        out.push_str(": ");
        write_value(out, value, depth + 1);
    }
    if !map.is_empty() {
        push_indent(out, depth - 1);
    }
    out.push('}');
}

/// picojson's string escaping: `"`, `\` and `/` are backslash-escaped, the usual control letters use their
/// short form, other bytes below 0x20 and 0x7F become `\u00xx`; bytes of 0x80 and above are copied.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
