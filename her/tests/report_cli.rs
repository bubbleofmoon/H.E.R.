//! Ticket 86: the executable report seam; all data is synthetic, no services.
#[path = "../src/install_root.rs"]
mod install_root;

use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = install_root::development_root(std::env::temp_dir().join(format!(
                "her-report-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        fs::create_dir_all(root.join("data/logs")).unwrap();
        Self(root)
    }
    fn write(&self, name: &str, rows: &[Value]) {
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        fs::write(self.0.join(name), text).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_her-cli"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.arg("report")
            .arg("--runtime-root")
            .arg(&self.0)
            .args(args)
            .env("HER_INSTALL_ROOT", self.0.join("unused-install"))
            .env_remove("HER_RUNTIME_ROOT")
            .env("TZ", "UTC")
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let out = self.run(&all);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
const TURN: &str = "turn-abcdefabcdefabcdefabcdef";

#[test]
fn never_extracts_next_model_from_echoed_provider_detail() {
    let f = Fixture::new();
    let t = now() - 10_000;
    f.write("data/run-record.jsonl", &[
        json!({"ts":t,"trigger":"turn","component":"model-chain","event":"candidates-exhausted","outcome":"failed","reason":"group:42: CONTENT_FILTER echoed input; next candidate NicknameSecret"}),
        json!({"ts":t,"trigger":"turn","component":"model-chain","event":"candidates-exhausted","outcome":"failed","reason":"group:42: CONTENT_FILTER echoed input; next candidate p/NicknameSecret"}),
        // HER appends the actual candidate after any provider-controlled detail.
        json!({"ts":t,"trigger":"turn","component":"model-chain","event":"failover-step","outcome":"ok","reason":"group:42: CONTENT_FILTER echoed input; next candidate NicknameSecret; next candidate p/b"}),
        json!({"ts":t,"trigger":"turn","component":"model-chain","event":"failover-step","outcome":"ok","reason":"group:42: CONTENT_FILTER detail; next candidate invalid model"}),
    ]);
    let r = f.json(&[]);
    let events = r["conversations"][0]["failoverEvents"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    assert!(events[0].get("nextModel").is_none());
    assert!(events[1].get("nextModel").is_none());
    assert_eq!(events[2]["nextModel"], "p/b");
    assert!(events[3].get("nextModel").is_none());
    let out = f.run(&[]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("p/b"));
    for secret in ["NicknameSecret", "echoed input", "invalid model"] {
        assert!(!r.to_string().contains(secret), "JSON disclosed {secret}");
        assert!(!text.contains(secret), "text disclosed {secret}");
    }
}

#[test]
fn cancelled_turn_uses_its_final_attempt_even_when_timing_follows_finalization() {
    for final_timing in [false, true] {
        let f = Fixture::new();
        let t = now() - 10_000;
        let second = "turn-eeeeeeeeeeeeeeeeeeeeeeee";
        f.write(
            "data/turn-ledger.jsonl",
            &[
                json!({"k":"begun","t":t,"turn":TURN,"key":"group:42"}),
                json!({"k":"attempt","t":t+100,"turn":TURN,"rpcId":"r1"}),
                json!({"k":"attempt","t":t+1100,"turn":TURN,"rpcId":"r2"}),
                json!({"k":"ended","t":t+4000,"turn":TURN,"outcome":"cancelled"}),
                json!({"k":"begun","t":t+4500,"turn":second,"key":"group:42"}),
                json!({"k":"attempt","t":t+4600,"turn":second,"rpcId":"r3"}),
                json!({"k":"ended","t":t+9000,"turn":second,"outcome":"completed"}),
            ],
        );
        let mut rows = vec![
            json!({"ts":t+1000,"key":"group:42","turnId":TURN,"attemptRpcId":"r1","model":"p/a","endKind":"error","attempts":1,"turnMs":1000}),
            json!({"ts":t+6500,"key":"group:42","turnId":second,"attemptRpcId":"r3","model":"p/c","endKind":"completed","attempts":1,"turnMs":2000}),
        ];
        if final_timing {
            // Cancellation is already terminal, and the next turn has begun and collected.
            rows.push(json!({"ts":t+7000,"key":"group:42","turnId":TURN,"attemptRpcId":"r2","model":"p/b","endKind":"aborted","attempts":2,"turnMs":5000}));
        }
        f.write("data/logs/turn-timing.jsonl", &rows);
        let r = f.json(&["--since", "1h", "--key", "group:42"]);
        let c = &r["conversations"][0];
        let first = &c["turns"]["entries"][0];
        assert_eq!(first["outcome"], "cancelled");
        if final_timing {
            assert_eq!(first["model"], "p/b");
            assert_eq!(first["endKind"], "aborted");
            assert_eq!(first["turnMs"], 5000.0);
            assert_eq!(c["turns"]["duration"]["sampledCount"], 2);
        } else {
            for field in ["model", "endKind", "turnMs"] {
                assert!(first[field].is_null(), "stale final {field}: {first}");
            }
            assert_eq!(c["turns"]["duration"]["sampledCount"], 1);
        }
        assert_eq!(c["turns"]["entries"][1]["model"], "p/c");
        assert_eq!(c["turns"]["entries"][1]["turnMs"], 2000.0);
        assert_eq!(c["timing"]["count"], if final_timing { 3 } else { 2 });
        let out = f.run(&["--since", "1h", "--key", "group:42"]);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let turn_line = text
            .lines()
            .find(|line| line.trim_start().starts_with("turn: ") && line.contains(TURN))
            .unwrap();
        assert!(!turn_line.contains("p/a"), "{turn_line}");
        if final_timing {
            assert!(
                turn_line.contains("p/b") && turn_line.contains("aborted"),
                "{turn_line}"
            );
        }
    }
}

#[test]
fn does_not_guess_a_cancelled_final_from_legacy_or_foreign_timing_rows() {
    let f = Fixture::new();
    let t = now() - 10_000;
    f.write(
        "data/turn-ledger.jsonl",
        &[
            json!({"k":"begun","t":t,"turn":TURN,"key":"group:42"}),
            json!({"k":"attempt","t":t+100,"turn":TURN,"rpcId":"r1"}),
            json!({"k":"attempt","t":t+1100,"turn":TURN,"rpcId":"r2"}),
            json!({"k":"ended","t":t+4000,"turn":TURN,"outcome":"cancelled"}),
        ],
    );
    f.write("data/logs/turn-timing.jsonl", &[
        json!({"ts":t+1000,"key":"group:42","model":"p/a","endKind":"error","attempts":1,"turnMs":1000}),
        json!({"ts":t+3000,"key":"group:42","model":"p/b","endKind":"error","attempts":2,"turnMs":3000}),
        json!({"ts":t+3500,"key":"group:42","turnId":"turn-ffffffffffffffffffffffff","attemptRpcId":"r2","model":"p/c","endKind":"aborted","attempts":2,"turnMs":3500}),
        json!({"ts":t+5000,"key":"group:42","model":"p/b","endKind":"aborted","attempts":2,"turnMs":5000}),
    ]);
    let r = f.json(&[]);
    let c = &r["conversations"][0];
    for field in ["model", "endKind", "turnMs"] {
        assert!(c["turns"]["entries"][0][field].is_null());
    }
    assert_eq!(c["turns"]["duration"]["sampledCount"], 0);
    assert_eq!(c["timing"]["count"], 4);
}

#[test]
fn streaming_filters_preserve_session_ownership_and_token_sequence_deduplication() {
    let f = Fixture::new();
    let t = now() - 10_000;
    fs::write(
        f.0.join("data/sessions.json"),
        json!({"sessions":{"group:42":{"sessionId":"live"}}}).to_string(),
    )
    .unwrap();
    f.write("data/token-usage.jsonl", &[
        json!({"k":"base","t":1,"sid":"historical","key":"group:42","seq":1}),
        json!({"k":"turn","t":t,"sid":"live","key":"private:43","seq":2,"turn":1,"step":1,"b":{"inputTokens":20,"cacheReadTokens":10,"outputTokens":4}}),
        json!({"k":"turn","t":t,"sid":"historical","seq":2,"turn":2,"step":1,"b":{"output":5}}),
        json!({"k":"turn","t":t,"sid":"live","seq":2,"turn":3,"step":1,"b":{"output":6}}),
    ]);
    f.write("data/token-usage-rust.jsonl", &[
        json!({"k":"turn","t":t+1,"sid":"live","key":"private:43","seq":3,"turn":1,"step":1,"b":{"cacheMiss":30,"cacheHit":10,"output":7}}),
        // A newer sequence outside the selected window must still replace the older sample.
        json!({"k":"turn","t":1,"sid":"live","seq":3,"turn":3,"step":1,"b":{"output":999}}),
    ]);
    let r = f.json(&["--since", "1h", "--key", "group:42"]);
    let c = &r["conversations"][0];
    let turns = c["modelTurns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    let live = turns.iter().find(|v| v["turn"] == 1).unwrap();
    assert_eq!(live["callCount"], 1);
    assert_eq!(live["calls"][0]["inputTokens"], 40.0);
    assert_eq!(live["calls"][0]["outputTokens"], 7.0);
    let historical = turns.iter().find(|v| v["turn"] == 2).unwrap();
    assert_eq!(historical["calls"][0]["outputTokens"], 5.0);
    assert!(!r.to_string().contains("private:43"));
}

#[test]
fn reports_turn_outcomes_timings_and_each_model_call_from_production_files() {
    let f = Fixture::new();
    let t = now() - 10_000;
    f.write(
        "data/turn-ledger.jsonl",
        &[
            json!({"k":"begun","t":t,"turn":TURN,"key":"group:42","epoch":"hidden"}),
            json!({"k":"ended","t":t+3000,"turn":TURN,"outcome":"completed","reason":"ok"}),
        ],
    );
    f.write("data/logs/turn-timing.jsonl", &[
        json!({"ts":t+3000,"key":"group:42","ok":true,"endKind":"completed","model":"local-relay/gemini-pro","attempts":1,"turnMs":3000}),
    ]);
    f.write("data/token-usage-rust.jsonl", &[
        json!({"k":"base","t":t,"sid":"s","key":"group:42","seq":1,"b":{"output":999}}),
        json!({"k":"turn","t":t+1000,"sid":"s","key":"group:42","seq":2,"turn":7,"step":1,"retry":0,"model":"gemini-pro","b":{"cacheMiss":100,"cacheHit":40,"output":12}}),
        json!({"k":"turn","t":t+2000,"sid":"s","key":"group:42","seq":3,"turn":7,"step":2,"retry":0,"model":"gemini-pro","b":{"cacheMiss":80,"cacheHit":60,"output":8}}),
    ]);
    let report = f.json(&[]);
    let c = &report["conversations"][0];
    assert_eq!(c["key"], "group:42");
    assert_eq!(c["turns"]["count"], 1);
    assert_eq!(c["turns"]["outcomes"]["completed"], 1);
    assert_eq!(c["timing"]["medianMs"], 3000.0);
    assert_eq!(c["timing"]["maxMs"], 3000.0);
    assert_eq!(c["timing"]["samples"][0]["endKind"], "completed");
    assert_eq!(c["timing"]["samples"][0]["model"], "local-relay/gemini-pro");
    assert_eq!(c["modelTurns"][0]["callCount"], 2);
    assert_eq!(c["modelTurns"][0]["calls"][0]["inputTokens"], 140.0);
    assert_eq!(c["modelTurns"][0]["calls"][1]["outputTokens"], 8.0);
    assert_eq!(report["sources"]["refusals"]["status"], "absent");
    let text = f.run(&[]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    for expected in [
        "group:42",
        "completed",
        "gemini-pro",
        "median",
        "refusals: absent",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn filters_the_window_and_key_but_keeps_current_pauses_and_reports_refusals_and_failover() {
    let f = Fixture::new();
    let t = now() - 10_000;
    let old = t - 3 * 3_600_000;
    let paused = "turn-bbbbbbbbbbbbbbbbbbbbbbbb";
    let resolved = "turn-cccccccccccccccccccccccc";
    f.write("data/turn-ledger.jsonl", &[
        json!({"k":"begun","t":old,"turn":paused,"key":"group:42"}),
        json!({"k":"unknown","t":t,"turn":paused,"reason":"send-unresolved","detail":"PRIVATE_CHAT"}),
        json!({"k":"begun","t":old,"turn":resolved,"key":"group:42"}),
        json!({"k":"unknown","t":old,"turn":resolved,"reason":"dsh-unreachable"}),
        json!({"k":"resolved","t":old+1,"turn":resolved,"action":"abandon"}),
        json!({"k":"begun","t":t,"turn":TURN,"key":"group:42"}),
        json!({"k":"attempt","t":t+100,"turn":TURN,"rpcId":"r1","sessionId":"s"}),
        json!({"k":"attempt","t":t+1100,"turn":TURN,"rpcId":"r2","sessionId":"s"}),
        json!({"k":"ended","t":t+3000,"turn":TURN,"outcome":"failed"}),
        json!({"k":"begun","t":t,"turn":"turn-dddddddddddddddddddddddd","key":"private:43"}),
    ]);
    f.write("data/logs/turn-timing.jsonl", &[
        json!({"ts":old,"key":"group:42","ok":true,"turnMs":99999}),
        json!({"ts":t+1000,"key":"group:42","ok":false,"endKind":"error","model":"p/a","attempts":1,"turnMs":1000}),
        json!({"ts":t+3000,"key":"group:42","ok":false,"endKind":"error","model":"p/b","attempts":2,"turnMs":9000}),
        json!({"ts":t+3000,"key":"private:43","turnMs":77777}),
    ]);
    let sample = json!({"k":"turn","t":t,"sid":"s","key":"group:42","seq":2,"turn":1,"step":1,"retry":0,"model":"p/b","b":{"cacheMiss":20,"cacheHit":10,"output":4}});
    f.write("data/token-usage.jsonl", &[sample.clone()]);
    let mut updated = sample.clone();
    updated["seq"] = json!(3);
    updated["b"]["output"] = json!(6);
    let mut old_sample = sample.clone();
    old_sample["t"] = json!(old);
    old_sample["turn"] = json!(2);
    let mut no_usage = sample.clone();
    no_usage["turn"] = json!(3);
    no_usage["b"] = Value::Null;
    f.write(
        "data/token-usage-rust.jsonl",
        &[updated.clone(), sample, updated, old_sample, no_usage],
    );
    f.write("data/logs/model-refusals.jsonl", &[
        json!({"time":t,"key":"group:42","backend":"dsh","provider":"local-relay","model":"gemini-pro","errorClass":"refusal","errorCode":"SAFETY","messages":[{"messageId":"987654321","text":"PRIVATE_CHAT"}]}),
        json!({"time":old,"key":"group:42","errorCode":"REFUSAL"}),
        json!({"time":t,"key":"private:43","errorCode":"REFUSAL"}),
    ]);
    let r = f.json(&["--since", "1h", "--key", "group:42"]);
    assert_eq!(r["conversations"].as_array().unwrap().len(), 1);
    let c = &r["conversations"][0];
    assert_eq!(c["turns"]["count"], 1);
    assert_eq!(c["turns"]["outcomes"]["failed"], 1);
    assert_eq!(c["timing"]["medianMs"], 5000.0);
    assert_eq!(c["timing"]["maxMs"], 9000.0);
    assert_eq!(c["pausedTurns"].as_array().unwrap().len(), 1);
    assert_eq!(c["pausedTurns"][0]["id"], paused);
    assert_eq!(c["pausedTurns"][0]["reason"], "send-unresolved");
    assert!(c["pausedTurns"][0]["ageMs"].as_i64().unwrap() >= 3 * 3_600_000);
    assert_eq!(c["failoverAttempts"].as_array().unwrap().len(), 1);
    assert_eq!(c["failoverAttempts"][0]["attempt"], 2);
    assert_eq!(c["refusals"].as_array().unwrap().len(), 1);
    assert_eq!(c["refusals"][0]["errorCode"], "SAFETY");
    assert_eq!(c["modelTurns"].as_array().unwrap().len(), 1);
    assert_eq!(c["modelTurns"][0]["callCount"], 1);
    assert_eq!(c["modelTurns"][0]["calls"][0]["outputTokens"], 6.0);
    let text = f.run(&["--since", "1h", "--key", "group:42"]);
    let text = String::from_utf8(text.stdout).unwrap();
    for expected in [paused, "send-unresolved", "SAFETY", "failover"] {
        assert!(text.contains(expected), "{text}");
    }
    for excluded in ["private:43", "PRIVATE_CHAT", "987654321"] {
        assert!(!r.to_string().contains(excluded));
        assert!(!text.contains(excluded));
    }
}

// Independent fixture calendar: walk years/months from the Unix epoch.
fn utc_stamp(at: i64) -> String {
    let secs = at / 1000;
    let mut days = secs / 86400;
    let mut year = 1970;
    let leap = |y: i64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    loop {
        let n = if leap(year) { 366 } else { 365 };
        if days < n {
            break;
        }
        days -= n;
        year += 1;
    }
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while days >= months[month] {
        days -= months[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        month + 1,
        days + 1,
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60
    )
}

#[test]
fn relay_summary_uses_the_same_window_and_tolerates_unknown_lines() {
    let f = Fixture::new();
    let t = now() - 10_000;
    // Synthetic lines following the documented GinLogrusLogger output, never actual logs:
    // https://github.com/router-for-me/CLIProxyAPI/blob/v8.0.36/internal/logging/gin_logger.go
    let stamp = utc_stamp(t);
    let old = utc_stamp(t - 2 * 3_600_000);
    let future = utc_stamp(t + 2 * 3_600_000);
    let path = f.0.join("relay.log");
    fs::write(&path, format!(
        "[{stamp}] [info ] | a1b2c3d4 | 200 | 23.559s | 127.0.0.1 | POST    \"/v1/chat/completions?token=RELAY_SECRET\"\n\
         [{stamp}] [warn ] | -------- | 429 | 450ms | 127.0.0.1 | POST    \"/v1/chat/completions\"\n\
         [{stamp}] [error] | a1b2c3d4 | 503 | 1m2s | 127.0.0.1 | POST    \"/v1/chat/completions\" | PRIVATE_CHAT\n\
         [{stamp}] [error] | a1b2c3d4 | 500 | 1500µs | 127.0.0.1 | POST    \"/v1/chat/completions\"\n\
         [{old}] [info ] | a1b2c3d4 | 200 | 999s | 127.0.0.1 | GET     \"/v1/models\"\n\
         [{future}] [info ] | a1b2c3d4 | 200 | 999s | 127.0.0.1 | GET     \"/v1/models\"\n\
         unknown log line RELAY_SECRET 987654321\n\
         [{stamp}] [error] | a1b2c3d4 | 502 | invalid-duration | 127.0.0.1 | POST \"/v1\"\n"
    )).unwrap();
    let r = f.json(&[
        "--since",
        "1h",
        "--key",
        "group:42",
        "--relay-log",
        path.to_str().unwrap(),
    ]);
    assert_eq!(r["sources"]["relay"]["status"], "present");
    assert_eq!(r["relay"]["requestCount"], 4);
    assert_eq!(
        r["relay"]["statusCodes"],
        json!({"200":1,"429":1,"500":1,"503":1})
    );
    assert_eq!(
        r["relay"]["durationsMs"],
        json!([23559.0, 450.0, 62000.0, 1.5])
    );
    assert_eq!(r["relay"]["medianMs"], 12004.5);
    assert_eq!(r["relay"]["maxMs"], 62000.0);
    assert_eq!(r["relay"]["unparsedLines"], 2);
    let output = f.run(&["--since", "1h", "--relay-log", path.to_str().unwrap()]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("relay: 4 requests"), "{text}");
    for secret in [
        "RELAY_SECRET",
        "PRIVATE_CHAT",
        "987654321",
        "127.0.0.1",
        "/v1/chat",
    ] {
        assert!(!text.contains(secret));
        assert!(!r.to_string().contains(secret));
    }
}

fn inventory(
    root: &std::path::Path,
) -> std::collections::BTreeMap<PathBuf, (bool, SystemTime, Vec<u8>)> {
    fn visit(
        base: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<PathBuf, (bool, SystemTime, Vec<u8>)>,
    ) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            out.insert(
                path.strip_prefix(base).unwrap().to_owned(),
                (meta.is_dir(), meta.modified().unwrap(), bytes),
            );
            if meta.is_dir() {
                visit(base, &path, out);
            }
        }
    }
    let mut out = Default::default();
    visit(root, root, &mut out);
    out
}

#[test]
fn shared_report_reads_do_not_write_or_disclose_payloads_even_with_damaged_input() {
    use std::io::Write;
    let f = Fixture::new();
    let t = now() - 10_000;
    fs::create_dir_all(f.0.join("config")).unwrap();
    fs::create_dir_all(f.0.join("data/locks")).unwrap();
    fs::write(
        f.0.join("config/config.json"),
        "invalid config OWNER_SECRET NicknameSecret 987654321",
    )
    .unwrap();
    fs::write(
        f.0.join("data/locks/turn-ledger-finalize.lock"),
        "sentinel-locked-by-production",
    )
    .unwrap();
    f.write("data/turn-ledger.jsonl", &[
        json!({"k":"begun","t":t,"turn":TURN,"key":"group:42","epoch":"EPOCH_SECRET","reason":"CHAT_SECRET","inbound":["987654321"]}),
        json!({"k":"tools","t":t,"turn":TURN,"tools":[{"text":"CHAT_SECRET","nickname":"NicknameSecret","token":"TOKEN_SECRET"}]}),
        json!({"k":"unknown","t":t,"turn":TURN,"reason":"send-unresolved","detail":"CHAT_SECRET"}),
    ]);
    f.write("data/logs/turn-timing.jsonl", &[
        json!({"ts":t,"key":"group:42","model":"authorization:HEADER_SECRET","endKind":"CHAT_SECRET","error":"CHAT_SECRET","nickname":"NicknameSecret","token":"TOKEN_SECRET","qq":"987654321","ok":false,"turnMs":12}),
    ]);
    f.write("data/logs/model-refusals.jsonl", &[
        json!({"time":t,"key":"group:42","errorCode":"SAFETY","model":"valid-model","messages":[{"messageId":"987654321","nickname":"NicknameSecret","text":"CHAT_SECRET"}],"provider":"dsh-auth-COOKIE_SECRET"}),
    ]);
    let path = f.0.join("data/token-usage-rust.jsonl");
    fs::write(&path, b"not-json CHAT_SECRET\n{\"unfinished\":\"\xf0\x9f").unwrap();
    // A live writer keeps the file open; no exclusive-open or lock-based reader can work here.
    let mut opts = fs::OpenOptions::new();
    opts.read(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(1 | 2);
    }
    let mut writer = opts.open(&path).unwrap();
    writer.flush().unwrap();
    let before = inventory(&f.0);
    let report = f.json(&[]);
    assert_eq!(report["sources"]["tokens"]["skippedLines"], 2);
    assert_eq!(report["sources"]["tokens"]["status"], "present");
    let output = f.run(&[]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    for secret in [
        "CHAT_SECRET",
        "OWNER_SECRET",
        "NicknameSecret",
        "TOKEN_SECRET",
        "EPOCH_SECRET",
        "HEADER_SECRET",
        "COOKIE_SECRET",
        "987654321",
    ] {
        assert!(
            !report.to_string().contains(secret),
            "JSON disclosed {secret}"
        );
        assert!(!text.contains(secret), "text disclosed {secret}");
    }
    assert_eq!(
        inventory(&f.0),
        before,
        "report must not create, replace, truncate, chmod or rewrite runtime files"
    );
    drop(writer);
}

#[test]
fn correlates_turn_summaries_and_model_chain_events_without_printing_failure_details() {
    let f = Fixture::new();
    let t = now() - 30_000;
    let second = "turn-eeeeeeeeeeeeeeeeeeeeeeee";
    f.write(
        "data/turn-ledger.jsonl",
        &[
            json!({"k":"begun","t":t,"turn":TURN,"key":"group:42"}),
            json!({"k":"ended","t":t+9000,"turn":TURN,"outcome":"failed"}),
            json!({"k":"begun","t":t+10000,"turn":second,"key":"group:42"}),
            json!({"k":"ended","t":t+15000,"turn":second,"outcome":"completed"}),
        ],
    );
    f.write("data/logs/turn-timing.jsonl", &[
        json!({"ts":t+1000,"key":"group:42","model":"p/a","endKind":"error","attempts":1,"turnMs":1000}),
        json!({"ts":t+9000,"key":"group:42","model":"p/b","endKind":"error","attempts":2,"turnMs":9000}),
        json!({"ts":t+15000,"key":"group:42","model":"p/b","endKind":"completed","attempts":1,"turnMs":5000}),
    ]);
    f.write("data/run-record.jsonl", &[
        json!({"v":1,"seq":1,"ts":t+1000,"runId":"RUN_SECRET","trigger":"turn","component":"model-chain","event":"failover-step","outcome":"ok","reason":"group:42: CONTENT_FILTER CHAT_SECRET NicknameSecret 987654321; next candidate p/b"}),
        json!({"v":1,"seq":2,"ts":t+8000,"trigger":"turn","component":"model-chain","event":"failover-step","outcome":"ok","repeat":2,"firstTs":t+2000,"lastTs":t+7000,"reason":"group:42: CONTENT_FILTER CHAT_SECRET; next candidate p/b"}),
        json!({"v":1,"seq":3,"ts":t+9100,"trigger":"turn","component":"model-chain","event":"candidates-exhausted","outcome":"failed","reason":"group:42: CONTENT_FILTER CHAT_SECRET"}),
        json!({"ts":t-3*3_600_000,"trigger":"turn","component":"model-chain","event":"failover-step","reason":"group:42: OLD"}),
        json!({"ts":t,"trigger":"turn","component":"model-chain","event":"failover-step","reason":"private:43: PRIVATE_SECRET"}),
        json!({"ts":t,"trigger":"timer","component":"dsh","event":"probe","reason":"group:42: PRIVATE_SECRET"}),
    ]);
    f.write("data/logs/model-refusals.jsonl", &[
        json!({"time":t,"key":"group:42","errorClass":"refusal","errorCode":"CONTENT_FILTER","messages":[{"messageId":"987654321"}]}),
    ]);
    let r = f.json(&["--since", "1h", "--key", "group:42"]);
    let c = &r["conversations"][0];
    assert_eq!(c["turns"]["count"], 2);
    // One final cumulative timing sample per bridge turn: [9000,5000], not [1000,9000,5000].
    assert_eq!(c["turns"]["duration"]["medianMs"], 7000.0);
    assert_eq!(c["turns"]["duration"]["maxMs"], 9000.0);
    assert_eq!(c["turns"]["entries"][0]["model"], "p/b");
    assert_eq!(c["turns"]["entries"][0]["endKind"], "error");
    assert_eq!(c["timing"]["medianMs"], 5000.0);
    assert_eq!(r["sources"]["runRecord"]["status"], "present");
    assert_eq!(c["failoverEvents"].as_array().unwrap().len(), 3);
    assert_eq!(c["failoverEvents"][0]["nextModel"], "p/b");
    assert_eq!(c["failoverEvents"][1]["repeat"], 2);
    assert_eq!(c["failoverEvents"][1]["at"], t + 7000);
    assert_eq!(c["failoverEvents"][2]["event"], "candidates-exhausted");
    assert_eq!(c["refusals"][0]["errorCode"], "CONTENT_FILTER");
    let output = f.run(&["--since", "1h", "--key", "group:42"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("median 7000.0 ms"), "{text}");
    assert!(text.contains("candidates-exhausted"), "{text}");
    for secret in [
        "CHAT_SECRET",
        "NicknameSecret",
        "987654321",
        "PRIVATE_SECRET",
        "RUN_SECRET",
        "private:43",
    ] {
        assert!(!r.to_string().contains(secret));
        assert!(!text.contains(secret));
    }
}
