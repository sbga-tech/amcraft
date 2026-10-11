use std::io::Read;

use aes::Aes256;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use flate2::read::ZlibDecoder;

use super::*;
use crate::abaas::wire::{HEADER_LEN, derive_key, encode_response};
use crate::mock::SimClock;
use backend::mock::{SimFs, SimHttp};

const FOLDER: &str = r"Y:\SDEY\abaas\";

struct Rig {
    dir: tempfile::TempDir,
    clock: SimClock,
    http: SimHttp,
    paths: SimFs,
    engine: Engine,
}

fn sys_config() -> SysConfig {
    SysConfig {
        game_id: "sdey".to_owned(),
        major: 1,
        minor: 5,
        location: 1234,
        keychip: "ABCD0123456".to_owned(),
        main_id: "EFGH0123456".to_owned(),
        folder: FOLDER.to_owned(),
    }
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let paths = SimFs::new(dir.path());
    std::fs::create_dir_all(paths.map(FOLDER)).unwrap();
    let mut engine = Engine::default();
    assert!(engine.set_config(&sys_config(), &paths));
    Rig { dir, clock: SimClock::new(), http: SimHttp::new(), paths, engine }
}

impl Rig {
    fn update(&mut self, times: usize) {
        for _ in 0..times {
            let mut http = self.http.clone();
            let mut env = Env { clock: &self.clock, http: &mut http, paths: &self.paths };
            self.engine.update(&mut env);
        }
    }

    fn answer(&self, json: &str) {
        let key = derive_key("SDEY");
        self.http.push_answer(Ok(backend::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: encode_response(&key, 0, json.as_bytes()),
        }));
    }

    fn write_log(&self, relative: &str, content: &[u8]) {
        let host = self.paths.map(&format!("{FOLDER}{relative}"));
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(host, content).unwrap();
    }
}

/// The JSON a request carries: the body without header, decrypted with the IV = header and inflated.
fn request_json(body: &[u8]) -> String {
    let key = derive_key("SDEY");
    let iv: [u8; HEADER_LEN] = body[..HEADER_LEN].try_into().unwrap();
    let plain = cbc::Decryptor::<Aes256>::new((&key).into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&body[HEADER_LEN..])
        .unwrap();
    let mut json = String::new();
    ZlibDecoder::new(plain.as_slice()).read_to_string(&mut json).unwrap();
    json
}

#[test]
fn set_config_rejects_each_invalid_field_and_keeps_the_previous_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let paths = SimFs::new(dir.path());
    std::fs::create_dir_all(paths.map(FOLDER)).unwrap();
    let mut engine = Engine::default();
    let broken = [
        SysConfig { game_id: "SDE".to_owned(), ..sys_config() },
        SysConfig { major: 100, ..sys_config() },
        SysConfig { minor: 100, ..sys_config() },
        SysConfig { keychip: "SHORT".to_owned(), ..sys_config() },
        SysConfig { main_id: "TOOLONG0123456".to_owned(), ..sys_config() },
        SysConfig { folder: r"Y:\a|b\".to_owned(), ..sys_config() },
        SysConfig { folder: r"Y:\missing\".to_owned(), ..sys_config() },
        SysConfig { folder: String::new(), ..sys_config() },
    ];
    for config in &broken {
        assert!(!engine.set_config(config, &paths), "{config:?}");
        assert!(!engine.is_configured());
    }
    assert!(engine.set_config(&sys_config(), &paths));
    assert!(!engine.set_config(&broken[0], &paths));
    assert!(engine.is_configured());
}

#[test]
fn polling_posts_the_encrypted_file_list_and_adopts_the_answer() {
    let mut rig = rig();
    rig.write_log("a.log", b"one");
    rig.answer(r#"{"settings":{"interval":100,"max_bandwidth":5000,"max_datasize":30}}"#);
    assert_eq!(rig.engine.file_count(), -1);
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(8);

    let requests = rig.http.requests();
    assert_eq!(requests[0].url, "http://SDEY.amlog.sys-all.net/api/polling");
    assert_eq!(requests[0].method, Method::Post);
    assert!(requests[0].headers.contains(&("User-Agent".to_owned(), "ABS/SDEY/ABCD0123456".to_owned())));
    assert_eq!(&requests[0].body[..8], &[2, 1, 1, 5, b'S', b'D', b'E', b'Y']);
    let json = request_json(&requests[0].body);
    assert!(json.starts_with(r#"{"client":{"serial":"ABCD0123456","main_id":"EFGH0123456","game_id":"SDEY","game_ver":"1.05","location_id":1234,"create_time":"20240514160"#), "{json}");
    assert!(json.contains(r#""files":[{"path":"a.log","size":3,"mtime":""#), "{json}");

    // A succeeded poll without instructions: nothing to upload, and the server is alive.
    assert_eq!(rig.engine.file_count(), 0);
    assert!(rig.engine.alive());
    // The interval 100 is clamped to 1800, the bandwidth to 2048 and the size limit is taken.
    assert_eq!(rig.engine.setting, Setting { interval: 1800, max_bandwidth: 2048, max_datasize: 30 });
}

#[test]
fn an_upload_instruction_sends_one_segment_and_is_removed_after_the_last() {
    let mut rig = rig();
    rig.write_log("sub/b.log", b"abc");
    rig.answer(r#"{"upload":[{"path":"sub\\b.log","next_segment":1}]}"#);
    rig.answer("{}");
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(16);

    let requests = rig.http.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[1].url, "http://SDEY.d-amlog.sys-all.net/up/logdata");
    let json = request_json(&requests[1].body);
    assert!(json.contains(r#""file":{"path":"sub\\b.log","size":3,"mtime":""#), "{json}");
    assert!(json.contains(r#""md5":"900150983CD24FB0D6963F7D28E17F72","#), "{json}");
    assert!(json.ends_with(r#""segment_size":3,"body":"YWJj\r\n"}}"#), "{json}");
    assert_eq!(rig.engine.file_count(), 0);
}

#[test]
fn a_server_failure_retries_after_the_table_wait() {
    let mut rig = rig();
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(8);
    // No answer is queued: the poll fails, the retry wait of 60 s starts.
    assert_eq!(rig.http.requests().len(), 1);
    assert!(!rig.engine.alive());
    assert_eq!(rig.engine.file_count(), -1);
    rig.clock.advance_ms(59_000);
    rig.update(4);
    assert_eq!(rig.http.requests().len(), 1);
    rig.clock.advance_ms(2_000);
    rig.update(8);
    assert_eq!(rig.http.requests().len(), 2);
    // The second failure waits 120 s.
    rig.clock.advance_ms(100_000);
    rig.update(8);
    assert_eq!(rig.http.requests().len(), 2);
    rig.clock.advance_ms(21_000);
    rig.update(8);
    assert_eq!(rig.http.requests().len(), 3);
}

#[test]
fn error_messages_are_queued_and_sent_in_notices_of_ten() {
    let mut rig = rig();
    let clock = SimClock::new();
    assert_eq!(Engine::default().set_error_message("x", &clock), -3);
    assert_eq!(rig.engine.set_error_message("", &rig.clock), -2);
    assert_eq!(rig.engine.set_error_message(&"x".repeat(MAX_MESSAGE + 1), &rig.clock), -2);
    for index in 0..12 {
        assert_eq!(rig.engine.set_error_message(&format!("error {index}"), &rig.clock), 0);
    }
    rig.answer("{}");
    rig.answer("{}");
    rig.answer("{}");
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(24);

    let requests = rig.http.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(requests[1].url, "http://SDEY.amlog.sys-all.net/api/notice");
    let first = request_json(&requests[1].body);
    assert!(first.contains(r#""content":"error 0""#) && first.contains(r#""content":"error 9""#), "{first}");
    assert!(!first.contains("error 10"), "{first}");
    let second = request_json(&requests[2].body);
    assert!(second.contains(r#""content":"error 10""#) && second.contains(r#""content":"error 11""#), "{second}");
}

#[test]
fn the_application_queue_holds_one_hundred_messages() {
    let mut rig = rig();
    for _ in 0..100 {
        assert_eq!(rig.engine.set_error_message("e", &rig.clock), 0);
    }
    assert_eq!(rig.engine.set_error_message("e", &rig.clock), -1);
    // The overflow is reported through the library queue, which is uploaded first.
    rig.answer("{}");
    rig.answer("{}");
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(16);
    let json = request_json(&rig.http.requests()[1].body);
    assert!(json.contains(r#""content":"[LIB MESSAGE] 100 error messages were detected.""#), "{json}");
}

#[test]
fn a_missing_log_folder_fails_the_poll_and_reports_it() {
    let mut rig = rig();
    std::fs::remove_dir_all(rig.paths.map(FOLDER)).unwrap();
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(2);
    assert!(rig.http.requests().is_empty());
    assert_eq!(rig.engine.lib_messages.len(), 1);
    assert_eq!(rig.engine.lib_messages[0].content, format!("[LIB MESSAGE] cannot open {FOLDER}*"));
    let _ = &rig.dir;
}

#[test]
fn a_server_delete_instruction_removes_only_an_unchanged_file() {
    let mut rig = rig();
    rig.write_log("old.log", b"x");
    rig.write_log("new.log", b"y");
    let host_old = rig.paths.map(&format!("{FOLDER}old.log"));
    let stamp = files::file_stamp(std::fs::metadata(&host_old).unwrap().modified().unwrap());
    rig.answer(&format!(
        r#"{{"delete":[{{"path":"old.log","mtime":"{stamp}"}},{{"path":"new.log","mtime":"19990101000000"}}]}}"#
    ));
    rig.engine.allow_execution(true, rig.clock.tick_ms());
    rig.update(16);
    assert!(!host_old.exists());
    assert!(rig.paths.map(&format!("{FOLDER}new.log")).exists());
    assert!(rig.engine.deletes.is_empty());
}
