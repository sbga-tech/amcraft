//! The ABaaS request bodies and the polling answer (`spec/libam/storage.md` 6.9.7, 6.9.8).
//!
//! Requests are compact JSON with the member order of the original (RapidJSON `Writer`), so they are written by
//! hand: `serde_json` would sort or reorder nothing here, but its escaping of control characters differs
//! (lower-case hex).

use std::fmt::Write as _;

use serde_json::Value;

use super::engine::{Client, Message, Setting};
use super::files::FileEntry;

/// One upload segment, ready to serialise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Segment {
    pub path: String,
    pub size: u64,
    pub mtime: String,
    pub md5: String,
    pub total_segment: u32,
    pub current_segment: u32,
    pub segment_size: u64,
    /// Base64 text with CRLF line breaks.
    pub body: String,
}

/// RapidJSON string escaping: `"` `\` and the control characters; everything else unescaped UTF-8.
fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_member(out: &mut String, key: &str, value: &str) {
    push_string(out, key);
    out.push(':');
    push_string(out, value);
}

/// `Postjson` (`0x140383D70`): the open object with its `client` member, no closing brace.
fn prefix(client: &Client, create_time: &str) -> String {
    let mut out = String::from("{\"client\":{");
    push_member(&mut out, "serial", &client.serial);
    out.push(',');
    push_member(&mut out, "main_id", &client.main_id);
    out.push(',');
    push_member(&mut out, "game_id", &client.game_id);
    out.push(',');
    push_member(&mut out, "game_ver", &format!("{}.{:02}", client.major, client.minor));
    let _ = write!(out, ",\"location_id\":{},", client.location);
    push_member(&mut out, "create_time", create_time);
    out.push('}');
    out
}

/// `FileListPostjson` (`0x14037E0B0`).
pub(super) fn polling(client: &Client, create_time: &str, setting: &Setting, files: &[FileEntry]) -> String {
    let mut out = prefix(client, create_time);
    let _ = write!(
        out,
        ",\"settings\":{{\"interval\":{},\"max_bandwidth\":{},\"max_datasize\":{}}},\"files\":[",
        setting.interval, setting.max_bandwidth, setting.max_datasize
    );
    for (index, file) in files.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('{');
        push_member(&mut out, "path", &file.path);
        let _ = write!(out, ",\"size\":{},", file.size);
        push_member(&mut out, "mtime", &file.mtime);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// `ErrInfoPostjson` (`0x140381A00`).
pub(super) fn notice(client: &Client, create_time: &str, messages: &[Message]) -> String {
    let mut out = prefix(client, create_time);
    out.push_str(",\"messages\":[");
    for (index, message) in messages.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('{');
        push_member(&mut out, "content", &message.content);
        out.push(',');
        push_member(&mut out, "create_time", &message.create_time);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// `FilePostjson` (`0x140382140`).
pub(super) fn upload(client: &Client, create_time: &str, segment: &Segment) -> String {
    let mut out = prefix(client, create_time);
    out.push_str(",\"file\":{");
    push_member(&mut out, "path", &segment.path);
    let _ = write!(out, ",\"size\":{},", segment.size);
    push_member(&mut out, "mtime", &segment.mtime);
    out.push(',');
    push_member(&mut out, "md5", &segment.md5);
    let _ = write!(
        out,
        ",\"total_segment\":{},\"current_segment\":{},\"segment_size\":{},",
        segment.total_segment, segment.current_segment, segment.segment_size
    );
    push_member(&mut out, "body", &segment.body);
    out.push_str("}}");
    out
}

/// The members of a polling answer (`ResponseJson::analyze`, `0x140380CE0`); every one is optional.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PollingAnswer {
    pub interval: Option<u64>,
    pub max_bandwidth: Option<u64>,
    pub max_datasize: Option<u64>,
    /// `Some` replaces the upload list.
    pub upload: Option<Vec<(String, u32)>>,
    /// `Some` is appended to the delete list.
    pub delete: Option<Vec<(String, String)>>,
}

/// Parses a polling answer; `None` for text that is not JSON (the poll still counts as successful).
pub(super) fn analyze(json: &[u8]) -> Option<PollingAnswer> {
    let root: Value = serde_json::from_slice(json).ok()?;
    let setting = |key: &str| root.get("settings").and_then(|settings| settings.get(key)).and_then(Value::as_u64);
    let upload = root.get("upload").and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|item| {
                let path = item.get("path")?.as_str()?;
                let next = u32::try_from(item.get("next_segment")?.as_u64()?).ok()?;
                Some((path.to_owned(), next))
            })
            .collect()
    });
    let delete = root.get("delete").and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|item| Some((item.get("path")?.as_str()?.to_owned(), item.get("mtime")?.as_str()?.to_owned())))
            .collect()
    });
    Some(PollingAnswer {
        interval: setting("interval"),
        max_bandwidth: setting("max_bandwidth"),
        max_datasize: setting("max_datasize"),
        upload,
        delete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::for_test("KEYCHIP0001", "MAINBRD0001", "SDEY", 1, 5, 1234)
    }

    #[test]
    fn polling_request_keeps_the_member_order() {
        let setting = Setting::default();
        let files = [FileEntry { path: r"a\b.log".into(), size: 7, mtime: "20240514160000".into() }];
        let json = polling(&client(), "20240514160001", &setting, &files);
        assert_eq!(
            json,
            r#"{"client":{"serial":"KEYCHIP0001","main_id":"MAINBRD0001","game_id":"SDEY","game_ver":"1.05","location_id":1234,"create_time":"20240514160001"},"settings":{"interval":3600,"max_bandwidth":1024,"max_datasize":20},"files":[{"path":"a\\b.log","size":7,"mtime":"20240514160000"}]}"#
        );
    }

    #[test]
    fn strings_escape_like_rapidjson() {
        let mut out = String::new();
        push_string(&mut out, "a\"b\\c\n\u{1}\u{7f}é");
        assert_eq!(out, "\"a\\\"b\\\\c\\n\\u0001\u{7f}é\"");
    }

    #[test]
    fn upload_body_member_is_last_and_crlf_is_escaped() {
        let segment = Segment {
            path: "x.log".into(),
            size: 3,
            mtime: "20240101000000".into(),
            md5: "0".repeat(32),
            total_segment: 1,
            current_segment: 1,
            segment_size: 3,
            body: "QUJD\r\n".into(),
        };
        let json = upload(&client(), "20240514160001", &segment);
        assert!(json.ends_with(r#""segment_size":3,"body":"QUJD\r\n"}}"#), "{json}");
    }

    #[test]
    fn polling_answer_replaces_uploads_and_ignores_missing_members() {
        let answer = analyze(
            br#"{"settings":{"interval":1,"max_bandwidth":5000},"upload":[{"path":"a","next_segment":2},{"x":1}],"delete":[{"path":"b","mtime":"20240101000000"}]}"#,
        )
        .unwrap();
        assert_eq!(answer.interval, Some(1));
        assert_eq!(answer.max_bandwidth, Some(5000));
        assert_eq!(answer.max_datasize, None);
        assert_eq!(answer.upload, Some(vec![("a".to_owned(), 2)]));
        assert_eq!(answer.delete, Some(vec![("b".to_owned(), "20240101000000".to_owned())]));
        assert_eq!(analyze(b"{}").unwrap(), PollingAnswer::default());
        assert!(analyze(b"").is_none());
    }
}
