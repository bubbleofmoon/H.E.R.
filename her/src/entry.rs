mod dsh;
mod dsh_credential;
mod digest;
mod loopback_http;
mod onebot;
mod outbound_message;
mod mcp;
mod aux_text;
mod aux_fetch;
mod aux_search;
mod aux_mcp;
mod dsh_mcp_config;
mod dsh_profile;
mod qq_tools;
mod model_view;
mod model_text_policy;
mod model_failure;
mod mcp_validate;
mod memory;
mod config_tx;
mod social_state;
mod conversation_state;
mod memory_actions;
mod pipeline;
mod harness;
mod turn_engine;
mod engine_switch;
mod prompt;
mod prompt_text;
mod briefing;
mod briefing_delivery;
mod initial_prompts;
mod persona;
mod persona_command;
mod wake_words;
mod activity;
mod feedback;
mod wake;
mod question;
#[cfg(test)]
mod wake_rate_tests;
#[cfg(test)]
mod production_root_tests;
mod session;
mod live_bridge;
pub(crate) use live_bridge::{
    append_runtime_json_line, decide, event_message_id, feed_slang, names_self, now_ms,
    parse_target, plain_text, protocol_integer, reserved2, sync_wake_configs, text_seed,
    wake_cfg_of, Event, Triggers,
};
mod reserved;
mod sticker;
mod failover;
mod console;
mod console_auth;
mod console_ui;
mod desktop;
mod qq_login;
mod pricing;
mod ledger;
mod report;
mod role;
mod social_mem;
mod slang;
mod slang_learn;
mod wait_util;
mod backend;
mod safe_fetch;
mod self_avatar;
mod media_normalize;
mod media;
mod subprocess;
mod voice;
mod voice_stt;
mod forward_fmt;
mod preset_prompt;
mod runtime;
mod guarded_file;
mod guarded_fs;
mod scheduled_task;
mod release;
mod release_signature;
mod release_store;
mod update;
use her_memory::parity_clock;
mod config;
mod inbound;
mod send_outbox;
mod recorded_send;
mod group_mute;
mod history_log;
mod session_map;
mod session_context;
mod session_rotation;
mod turn_ledger;
mod turn_recovery;
mod chat;
mod inbound_text;
mod closed_agent;
mod lifecycle;
mod process_identity;
mod lifecycle_cli;
mod watchdog;
mod task_runs;
mod tasks;
mod local_time;
mod schedule;
mod backup;
mod restore;
mod console_tasks;
mod console_persona;
mod console_settings;
pub(crate) use console::front as console_front;
mod settings;
mod supervisor;
mod supervisor_dependencies;
mod dependency;
mod admin_alert;
mod run_record;
mod candidate_state;
mod recovery_log;
mod dsh_health;
mod health_check;
mod gateway_health;
mod dependency_watch;
mod snowluma_heal;
mod upstream_capture;

use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    subprocess::prevent_stdio_inheritance();
    let mut args: Vec<String> = std::env::args().collect();
    let install = match install_root::InstallRoot::select(&mut args) {
        Ok(install) => install,
        Err(error) => {
            eprintln!("[her] {error}");
            std::process::exit(2);
        }
    };
    // Set once, before Tokio starts threads, so selected children keep the same production boundary.
    std::env::set_var("HER_INSTALL_ROOT", subprocess::external_path(install.root())?);
    if args.get(1).map(String::as_str) == Some("install-root") {
        if args.get(2).map(String::as_str) == Some("layout") && args.len() == 3 {
            let layout = install
                .layout()
                .into_iter()
                .map(|(key, path)| {
                    subprocess::external_path(path)
                        .map(|value| (key.to_string(), serde_json::Value::String(value)))
                })
                .collect::<Result<serde_json::Map<String, Value>, String>>()?;
            println!("{}", Value::Object(layout));
            return Ok(());
        }
        if args.get(2).map(String::as_str) == Some("check-development")
            && args.len() == 5
            && args[3] == "--runtime-root"
        {
            if let Err(error) = install.check_development(std::path::Path::new(&args[4])) {
                eprintln!("[her] {error}");
                std::process::exit(2);
            }
            println!("{{\"ok\":true}}");
            return Ok(());
        }
        if args.get(2).map(String::as_str) == Some("check-supervisor")
            && args.len() == 7
            && args[3] == "--runtime-root"
            && args[5] == "--executable"
        {
            let check = || -> Result<(), String> {
                let image = std::fs::canonicalize(&args[6])
                    .map_err(|_| "resident executable is unavailable")?;
                if !image.is_file() {
                    return Err("resident executable is unavailable".into());
                }
                install.check_runtime(std::path::Path::new(&args[4]), &image, true)
            };
            if let Err(error) = check() {
                eprintln!("[her] {error}");
                std::process::exit(2);
            }
            println!("{{\"ok\":true}}");
            return Ok(());
        }
        eprintln!("usage: her-cli install-root layout [--install-root <absolute path>]\n       her-cli install-root check-development --runtime-root <absolute path>\n       her-cli install-root check-supervisor --runtime-root <absolute path> --executable <resident image>");
        std::process::exit(2);
    }
    run_entry(args)
}

#[tokio::main]
async fn run_entry(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if args.get(1).map(String::as_str) == Some("release") {
        std::process::exit(release::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("install") {
        std::process::exit(scheduled_task::install_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("scheduled-task") {
        std::process::exit(scheduled_task::run_cli(&args[2..]));
    }
    if args.len() == 1 || args.get(1).map(String::as_str) == Some("app") {
        std::process::exit(desktop::run_cli(args.get(2..).unwrap_or(&[])));
    }
    if args.get(1).map(String::as_str) == Some("dsh-mcp-config") {
        std::process::exit(dsh_mcp_config::run_cli(&args[2..]));
    }
    // #179: read-only check of the guarded DSH profile rows of a selected root.
    if args.get(1).map(String::as_str) == Some("dsh-profile") {
        std::process::exit(dsh_profile::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("report") {
        std::process::exit(report::run_cli(&args[2..]));
    }
    // #183: one-shot capture and denylist sweep of the next upstream model request (selected root).
    if args.get(1).map(String::as_str) == Some("upstream-capture") {
        std::process::exit(upstream_capture::run_cli(&args[2..]));
    }
    // Ticket 27: desired state and task history of a selected root (explicit root, works with a broken config).
    if args.get(1).map(String::as_str) == Some("process") {
        std::process::exit(process_identity::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("backup") {
        std::process::exit(restore::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("console") {
        std::process::exit(console_auth::run_cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("lifecycle") {
        std::process::exit(lifecycle_cli::run_lifecycle(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("stop") {
        std::process::exit(lifecycle_cli::run_lifecycle(&args[1..]));
    }
    if args.get(1).map(String::as_str) == Some("tasks") {
        std::process::exit(lifecycle_cli::run_tasks(&args[2..]));
    }
    // #186: admit or remove one conversation in its allow list and rollout.rustGroups at once.
    if args.get(1).map(String::as_str) == Some("conversation") {
        std::process::exit(lifecycle_cli::run_conversation(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("supervise") {
        std::process::exit(supervisor::run_cli(&args[2..]));
    }
    if args.get(1).is_some_and(|arg| !arg.starts_with("--")) {
        eprintln!("[her] unknown command");
        std::process::exit(2);
    }
    // Retired startup options must not fall through to implicit runtime paths.
    if args.iter().skip(1).any(|arg| {
        matches!(
            arg.as_str(),
            "--ab-full"
                | "--ab-probe"
                | "--shadow-turn"
                | "--shadow-state-root"
                | "--replay"
                | "--wake-replay"
                | "--replay-jsonl"
                | "--replay-triggers"
                | "--memory-probe"
                | "--memory-write-probe"
                | "--hot-render"
                | "--index-hint"
                | "--send-probe"
                | "--ask"
                | "--price-probe"
                | "--role-probe"
                | "--prompt-wrapped"
                | "--rpc-probe"
                | "--slang-parse-test"
                | "--selftest-401"
                | "--failover-probe"
                | "--sticker-sync"
                | "--build-prompt"
                | "--dsh-dump"
                | "--dsh-watch"
                | "--dsh-probe"
                | "--dsh-demo"
                | "--question-probe"
                | "--split-probe"
                | "--sensitive-probe"
                | "--backend-ask"
                | "--backends"
        )
    }) {
        eprintln!("[her] unknown option");
        std::process::exit(2);
    }
    let runtime_context = match runtime::RuntimeContext::from_process_args(&args) {
        Ok(context) => context,
        Err(message) => {
            eprintln!("[her] {message}");
            std::process::exit(2);
        }
    };
    activity::set_log_context(&runtime_context);

    // MCP server (stdio)
    if args.iter().skip(1).any(|a| a == "--mcp-serve") {
        if runtime_context.is_isolated() {
            return Ok(mcp::serve_with_context(&runtime_context)?);
        }
        return Ok(mcp::serve()?);
    }

    // Auxiliary MCP servers launched alongside `--mcp-serve`.
    if args.iter().skip(1).any(|a| a == "--mcp-web-search-serve") {
        return Ok(aux_mcp::serve_web_search(&runtime_context)?);
    }
    if args.iter().skip(1).any(|a| a == "--mcp-host-serve") {
        return Ok(aux_mcp::serve_host(&runtime_context)?);
    }

    // Console HTTP server
    if args.iter().skip(1).any(|a| a == "--console-serve") {
        let _console_owner = if runtime_context.is_isolated() {
            let gate_path = runtime_context.resolve_path(
                runtime::RuntimeArea::Data,
                "lifecycle/lifecycle.lock",
                runtime::PathIntent::WriteFile,
            )?;
            let _transition = lifecycle::lock_with_wait(&gate_path, std::time::Duration::from_secs(5))?;
            let desired = lifecycle::DesiredStore::new(runtime_context.data_dir().join("lifecycle"));
            match lifecycle::effective(&desired.read(), now_ms() as i64) {
                lifecycle::Effective::Act { wants, .. } if wants.console => {}
                _ => {
                    eprintln!("[her] desired state does not permit console startup");
                    std::process::exit(3);
                }
            }
            let path = runtime_context.resolve_path(
                runtime::RuntimeArea::Data,
                "logs/rbridge-console.lock",
                runtime::PathIntent::WriteFile,
            )?;
            let gate = lifecycle::OwnerGate::new(path);
            let identity = lifecycle::current_identity()?;
            match gate.acquire(
                "console",
                identity,
                None,
                now_ms() as i64,
                std::time::Duration::from_millis(500),
            ) {
                lifecycle::Acquire::Acquired(owner) => Some(owner),
                other => {
                    eprintln!("[her] console owner unavailable: {other:?}");
                    std::process::exit(3);
                }
            }
        } else {
            None
        };
        let requested_port = args
            .iter()
            .position(|arg| arg == "--console-port")
            .and_then(|index| args.get(index + 1));
        let port = if runtime_context.is_isolated() {
            match requested_port
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|port| *port != 0)
            {
                Some(port) => port,
                None => {
                    eprintln!("[her] isolated runtime requires an explicit non-zero console port");
                    std::process::exit(2);
                }
            }
        } else {
            requested_port
                .and_then(|value| value.parse().ok())
                .unwrap_or(3101)
        };
        if runtime_context.is_isolated() {
            let cfg = {
                let _config = social_state::acquire_config_gate(&runtime_context)?;
                if restore::recovery_pending(&runtime_context) {
                    json!({})
                } else {
                    if console_auth::reconcile_token_rotation(&runtime_context).is_err() {
                        crate::safe_eprintln!("[console] console credentials require recovery");
                    }
                    runtime_context.load_config().map_err(str::to_owned)?
                }
            };
            console::serve_with_context(&cfg, port, runtime_context)?;
        } else {
            let cfg = console::load_config();
            console::serve(&cfg, port)?;
        }
        return Ok(());
    }

    if !args.iter().skip(1).any(|a| a == "--live") {
        eprintln!("[her] specify --live for the live service");
        std::process::exit(2);
    }

    live_bridge::run(&runtime_context, &args).await
}
