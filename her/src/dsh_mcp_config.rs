//! `her dsh-mcp-config`: the DSH profile entries (`cordis.patch.yml`) for the three MCP servers of this bridge,
//! all pointing at this binary. It replaces the MCP part of the frozen `scripts/setup-dsh.mjs`, whose
//! `mcp-snowluma-host` and `mcp-web-search-safe` entries launched the old Node servers.
//!
//! Only the block between the `# === qq-bridge MCP BEGIN/END ===` markers is owned here. Everything outside it is
//! kept byte for byte; managed ids found outside the block (hand-written or from an older installer) are refused
//! instead of guessed at. Re-running is idempotent (an unchanged file is not rewritten); a changed file is saved
//! as `<file>.bak-<UTC timestamp>` first (newest five kept).

use std::path::{Path, PathBuf};

const BEGIN: &str = "# === qq-bridge MCP BEGIN ===";
const END: &str = "# === qq-bridge MCP END ===";
/// (entry id, serverName, bridge arguments, toolCallTimeoutMs). `serverName`s keep the tool names
/// (`mcp__snowluma__qq_*`, `mcp__web-search-safe__*`) that presets and guards already whitelist.
const SERVERS: [(&str, &str, &str, Option<u64>); 3] = [
    ("mcp-snowluma", "snowluma", "--mcp-serve", Some(725_000)),
    ("mcp-snowluma-host", "snowluma-host", "--mcp-host-serve", None),
    ("mcp-web-search-safe", "web-search-safe", "--mcp-web-search-serve", None),
];

fn yaml_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The managed block for `exe` (LF line endings, ends with a newline).
pub fn render_block(exe: &str, runtime: Option<(&str, u16)>) -> String {
    let mut out = format!("{BEGIN}\n# 由 her dsh-mcp-config 维护；这段区块会被整体替换，请勿手工编辑内部条目。\n");
    for (id, server_name, arg, timeout) in SERVERS {
        out.push_str("- insert:\n");
        out.push_str(&format!("    - id: {id}\n"));
        out.push_str("      name: '@deepseek-ai/dsh-mcp-client'\n");
        out.push_str("      config:\n");
        out.push_str(&format!("        serverName: {server_name}\n"));
        out.push_str("        transport: stdio\n");
        out.push_str(&format!("        command: {}\n", yaml_single_quote(exe)));
        out.push_str("        args:\n");
        out.push_str(&format!("          - {}\n", yaml_single_quote(arg)));
        if let Some((root, port)) = runtime {
            for argument in ["--runtime-root", root, "--console-port", &port.to_string()] {
                out.push_str(&format!("          - {}\n", yaml_single_quote(argument)));
            }
        }
        if let Some(ms) = timeout {
            // qq_wait_for_messages may wait ten minutes; DSH's default 60 s would cut the call short.
            out.push_str(&format!("        toolCallTimeoutMs: {ms}\n"));
        }
    }
    out.push_str(END);
    out.push('\n');
    out
}

fn managed_id_outside(line: &str) -> bool {
    let trimmed = line.trim_start().trim_start_matches("- ").trim_start();
    let Some(value) = trimmed.strip_prefix("id:") else { return false };
    let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
    SERVERS.iter().any(|(id, ..)| *id == value)
}

/// The patch file text after installing the block for `exe` into `existing`.
pub fn apply(existing: &str, exe: &str, runtime: Option<(&str, u16)>) -> Result<String, String> {
    let exe = crate::subprocess::external_path(exe)?;
    let root = runtime.map(|(root, _)| crate::subprocess::external_path(root)).transpose()?;
    let runtime = runtime.map(|(_, port)| (root.as_deref().unwrap(), port));
    let text = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let marker = |line: &str, which: &str| line.trim_end_matches(['\r', '\n']) == which;
    let begins: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| marker(l, BEGIN)).map(|(i, _)| i).collect();
    let ends: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| marker(l, END)).map(|(i, _)| i).collect();
    let block = render_block(&exe, runtime);
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => {
            if let Some(line) = lines.iter().find(|l| managed_id_outside(l)) {
                return Err(format!("managed MCP id outside the qq-bridge block ({}); remove it or restore the markers, nothing was changed", line.trim()));
            }
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block);
            Ok(out)
        }
        ([b], [e]) if b < e => {
            if let Some(line) = lines.iter().enumerate().filter(|(i, _)| i < b || i > e).map(|(_, l)| l).find(|l| managed_id_outside(l)) {
                return Err(format!("managed MCP id outside the qq-bridge block ({}); remove it, nothing was changed", line.trim()));
            }
            let mut out: String = lines[..*b].concat();
            out.push_str(&block);
            out.push_str(&lines[e + 1..].concat());
            Ok(out)
        }
        _ => Err("the qq-bridge MCP BEGIN/END markers are unbalanced or duplicated; nothing was changed".to_string()),
    }
}

pub(crate) fn utc_stamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}-{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, now.subsec_millis())
}

fn backup(file: &Path) -> Result<(), String> {
    let name = file.file_name().and_then(|n| n.to_str()).ok_or("patch file name is invalid")?;
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::copy(file, dir.join(format!("{name}.bak-{}", utc_stamp()))).map_err(|e| format!("backup failed: {e}"))?;
    let prefix = format!("{name}.bak-");
    let mut snaps: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    snaps.sort();
    let excess = snaps.len().saturating_sub(5);
    for old in &snaps[..excess] {
        let _ = std::fs::remove_file(dir.join(old));
    }
    Ok(())
}

fn write_atomic(file: &Path, text: &str) -> Result<(), String> {
    let temporary = PathBuf::from(format!("{}.{}.tmp", file.display(), std::process::id()));
    let result = std::fs::write(&temporary, text).and_then(|_| std::fs::rename(&temporary, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("write failed: {e}"))
}

/// `her dsh-mcp-config --patch-file <file> [--bridge-exe <path>] [--runtime-root <absolute root> --console-port <port>] [--check]`. Exit 0: installed / unchanged
/// (`--check`: already current); 1: `--check` and the file differs; 2: refused or failed.
pub fn run_cli(args: &[String]) -> i32 {
    let mut patch_file: Option<&str> = None;
    let mut exe: Option<String> = None;
    let mut check = false;
    let mut runtime_root: Option<&str> = None;
    let mut console_port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--patch-file" if patch_file.is_none() && i + 1 < args.len() => {
                patch_file = Some(&args[i + 1]);
                i += 1;
            }
            "--bridge-exe" if exe.is_none() && i + 1 < args.len() => {
                exe = Some(args[i + 1].clone());
                i += 1;
            }
            "--check" if !check => check = true,
            "--runtime-root" if runtime_root.is_none() && i + 1 < args.len() => {
                runtime_root = Some(&args[i + 1]);
                i += 1;
            }
            "--console-port" if console_port.is_none() && i + 1 < args.len() => {
                console_port = args[i + 1].parse::<u16>().ok().filter(|p| *p != 0);
                if console_port.is_none() {
                    eprintln!("dsh-mcp-config: --console-port must be 1-65535");
                    return 2;
                }
                i += 1;
            }
            other => {
                eprintln!("dsh-mcp-config: unsupported argument {other:?}; usage: dsh-mcp-config --patch-file <cordis.patch.yml> [--bridge-exe <path>] [--runtime-root <absolute root> --console-port <port>] [--check]");
                return 2;
            }
        }
        i += 1;
    }
    let runtime = match (runtime_root, console_port) {
        (None, None) => None,
        (Some(root), Some(port))
            if Path::new(root).is_absolute() && !root.contains(['\r', '\n']) =>
        {
            Some((root, port))
        }
        _ => {
            eprintln!("dsh-mcp-config: runtime selection requires an absolute --runtime-root and a non-zero --console-port together");
            return 2;
        }
    };
    let Some(patch_file) = patch_file else {
        eprintln!("dsh-mcp-config: --patch-file is required");
        return 2;
    };
    let exe = match exe.or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned())) {
        Some(exe) if !exe.trim().is_empty() => exe,
        _ => {
            eprintln!("dsh-mcp-config: the bridge executable path is unavailable; pass --bridge-exe");
            return 2;
        }
    };
    let file = Path::new(patch_file);
    let guard = || -> Result<(), String> {
        crate::install_root::check_development_path(file)?;
        if let Some((root, _)) = runtime {
            crate::install_root::check_development_path(Path::new(root))?;
        }
        Ok(())
    };
    if let Err(error) = guard() {
        eprintln!("dsh-mcp-config: {error}"); return 2;
    }
    let existing = match std::fs::read(file) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                eprintln!("dsh-mcp-config: {patch_file} is not UTF-8; nothing was changed");
                return 2;
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("dsh-mcp-config: cannot read {patch_file}: {e}");
            return 2;
        }
    };
    let updated = match apply(existing.as_deref().unwrap_or(""), &exe, runtime) {
        Ok(text) => text,
        Err(message) => {
            eprintln!("dsh-mcp-config: {message}");
            return 2;
        }
    };
    let unchanged = existing.as_deref().map(|e| e.strip_prefix('\u{feff}').unwrap_or(e)) == Some(updated.as_str());
    if check {
        println!("{}", if unchanged { "unchanged" } else { "differs" });
        return i32::from(!unchanged);
    }
    if unchanged {
        println!("unchanged");
        return 0;
    }
    if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("dsh-mcp-config: cannot create {}: {e}", parent.display());
            return 2;
        }
    }
    if existing.is_some() {
        if let Err(message) = backup(file) {
            eprintln!("dsh-mcp-config: {message}");
            return 2;
        }
    }
    match write_atomic(file, &updated) {
        Ok(()) => {
            println!("written");
            0
        }
        Err(message) => {
            eprintln!("dsh-mcp-config: {message}");
            2
        }
    }
}
