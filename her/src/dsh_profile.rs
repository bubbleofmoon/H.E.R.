//! #179: the DSH profile guard. While DSH serves chat, the production profile must keep the rows that stop the
//! harness prelude, its developer context, `AGENTS.md` injection and the local shell and file tools, and the presets
//! must carry persona text with no #142 denied term. HER writes its own rows into the profile's patch layer at start
//! (`restore`: the supervisor before it starts DSH, and the live bridge), checks the profile and the preset bundle
//! before every DSH prompt (`admit`) and, while anything is missing, refuses the prompt before any DSH call and
//! alerts the owner once per distinct gap. The guard applies to the install root's runtime (production); development
//! roots outside it and the fixed-path legacy mode (retired by #78) are not guarded.
//!
//! The YAML reader here is a small line reader for the block style DSH profiles and presets use. A form it does not
//! read (flow collections other than `{}`/`[]`, anchors, aliases, tabs, nested sequences, document markers) is itself
//! a refusal, and every guarded plugin package named on a non-comment line must be a row it read.

use crate::run_record::{Component, Outcome, Trigger};
use crate::runtime::{PathIntent, RuntimeArea, RuntimeContext};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// The profile `dsh web` loads: its own patch layer, applied after every bundle layer.
pub(crate) const PROFILE_FILE: &str = "profiles/web/cordis.patch.yml";
const PROFILE_DIR: &str = "profiles/web";
/// The bundle that inserts HER's agent presets; `package.json` `dsh.bundle.patch` lists its preset files.
pub(crate) const PRESET_BUNDLE: &str = "bundles/qq-agent-presets";
pub(crate) const BEGIN: &str = "# === her profile guard BEGIN ===";
pub(crate) const END: &str = "# === her profile guard END ===";
/// Start of the error a refused prompt returns. Neutral wording with no denied term.
pub(crate) const REFUSAL_PREFIX: &str = "对话后端配置未通过检查，回合未提交：";
const BACKUP_PREFIX: &str = "cordis.patch.yml.bak-her-";
const BACKUPS_KEPT: usize = 5;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PRESET_FILES: usize = 16;
const LOCK_FILE: &str = "locks/dsh-profile.lock";
/// The problem codes the owner was last alerted about (data area); absent while everything passes.
const ALERT_FILE: &str = "dsh-profile-alert.txt";
const USAGE: &str = "usage: her-cli dsh-profile check --runtime-root <root>";

const TOOL_IDS: &[&str] = &[
    "tool-bash",
    "tool-pwsh",
    "tool-fs",
    "tool-fs-search",
    "tool-terminal",
    "persistent-bash",
    "persistent-pwsh",
];
const TOOL_PACKAGES: &[&str] = &[
    "@deepseek-ai/dsh-tool-bash",
    "@deepseek-ai/dsh-tool-bash-persistent",
    "@deepseek-ai/dsh-tool-pwsh",
    "@deepseek-ai/dsh-tool-pwsh-persistent",
    "@deepseek-ai/dsh-tool-terminal",
    "@deepseek-ai/dsh-tool-fs",
    "@deepseek-ai/dsh-tool-fs-search",
];
const DEV_CONTEXT_PACKAGE: &str = "@deepseek-ai/dsh-harness-dev-context";
const SYSTEM_PROMPT_PACKAGE: &str = "@deepseek-ai/dsh-system-prompt";
const AGENT_INSTRUCTIONS_PACKAGE: &str = "@deepseek-ai/dsh-agent-instructions";
const PERSONA_PACKAGE: &str = "@deepseek-ai/dsh-persona";
/// Problem code and the packages whose every mention must be a row the reader read.
const GUARDED_PACKAGES: &[(&str, &[&str])] = &[
    ("local-tools", TOOL_PACKAGES),
    ("dev-context", &[DEV_CONTEXT_PACKAGE]),
    ("system-prompt", &[SYSTEM_PROMPT_PACKAGE]),
    ("agent-instructions", &[AGENT_INSTRUCTIONS_PACKAGE]),
];

/// HER's rows. A patch replaces the keys it names on the row with that id, and a later patch wins, so the block is
/// kept last in the profile's own patch layer.
const ROWS: &str = "- id: system-prompt\n  config:\n    includeHarnessIdentity: false\n    includeRuntimeContext: false\n    personaPrefix: ''\n    personaSuffix: ''\n- id: harness-dev-context\n  disabled: true\n- id: agent-instructions\n  disabled: true\n- id: tool-bash\n  disabled: true\n- id: tool-pwsh\n  disabled: true\n- id: tool-fs\n  disabled: true\n- id: tool-fs-search\n  disabled: true\n";

/// The managed block (LF line endings, ends with a newline).
pub(crate) fn render_block() -> String {
    format!("{BEGIN}\n# 由 HER 维护：启动时整段写回并放在文件最后，请勿手工编辑内部条目。\n{ROWS}{END}\n")
}

/// The profile patch text after installing HER's block into `existing`: any old block is taken out, everything
/// else keeps its bytes and order, and the block goes last. DSH's new-profile template (only `[]` besides comments)
/// loses its `[]`, which a block list cannot follow.
pub(crate) fn apply_rows(existing: &str) -> Result<String, String> {
    let text = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let marker = |line: &str, which: &str| line.trim_end_matches(['\r', '\n']) == which;
    let begins: Vec<usize> = (0..lines.len()).filter(|i| marker(lines[*i], BEGIN)).collect();
    let ends: Vec<usize> = (0..lines.len()).filter(|i| marker(lines[*i], END)).collect();
    let outside: Vec<&str> = match (begins.as_slice(), ends.as_slice()) {
        ([], []) => lines.clone(),
        ([begin], [end]) if begin < end => lines[..*begin].iter().chain(&lines[end + 1..]).copied().collect(),
        _ => return Err("the profile guard markers are unbalanced or duplicated; nothing was changed".to_string()),
    };
    let only_template = outside
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .all(|line| line == "[]");
    let mut out: String = outside.iter().filter(|line| !(only_template && line.trim() == "[]")).copied().collect();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&render_block());
    Ok(out)
}

/// Whether the guard applies: the install root's runtime only (a selected root inside the configured install root).
fn applies(context: &RuntimeContext) -> bool {
    if !context.is_isolated() {
        return false;
    }
    let Some(root) = context.runtime_root() else { return false };
    #[cfg(test)]
    if test_support::FORCED.lock().unwrap_or_else(|p| p.into_inner()).contains(root) {
        return true;
    }
    crate::install_root::InstallRoot::configured()
        .and_then(|install| install.protects(root))
        .unwrap_or(true)
}

/// The runtime an alert belongs to (test capture).
#[cfg(test)]
fn scope(context: &RuntimeContext) -> String {
    context.runtime_root().map(|root| root.to_string_lossy().into_owned()).unwrap_or_default()
}

fn record(context: &RuntimeContext, trigger: Trigger, event: &str, outcome: Outcome, reason: &str) {
    crate::recovery_log::record(context, trigger, Component::Dsh, event, outcome, reason);
}

/// Refuse a DSH prompt while a guarded row is missing. The owner is alerted once per distinct set of problems; the
/// set is kept in the data area, so a restart with the same gap does not alert again.
pub(crate) fn admit(context: &RuntimeContext) -> Result<(), String> {
    if !applies(context) {
        return Ok(());
    }
    let alerted = context.read_optional_text(RuntimeArea::Data, ALERT_FILE).ok().flatten();
    match check(context) {
        Ok(()) => {
            if alerted.is_some() {
                let _ = context.remove_file(RuntimeArea::Data, ALERT_FILE);
                record(context, Trigger::Turn, "profile-guard", Outcome::Ok, "every guarded profile row is in place again");
            }
            Ok(())
        }
        Err(problems) => {
            let codes = problems.join(", ");
            if alerted.as_deref() != Some(codes.as_str()) {
                let _ = context.write_atomic(RuntimeArea::Data, ALERT_FILE, codes.as_bytes());
                record(
                    context,
                    Trigger::Turn,
                    "profile-guard",
                    Outcome::Failed,
                    &format!("prompts are refused until the profile is fixed: {codes}"),
                );
                alert(context, &codes);
            }
            Err(format!("{REFUSAL_PREFIX}{codes}"))
        }
    }
}

fn alert(context: &RuntimeContext, codes: &str) {
    let text = format!("⚠️ 对话后端配置检查未通过（{codes}），新回合暂停提交。HER 启动时会写回它维护的配置行，其余需手工修复。");
    #[cfg(test)]
    test_support::ALERTS.lock().unwrap_or_else(|p| p.into_inner()).push((scope(context), text));
    #[cfg(not(test))]
    {
        let cfg = context.load_config().unwrap_or_default();
        let _ = crate::admin_alert::notify_notice(context, &cfg, &text, &format!("dsh-profile-guard:{codes}"));
    }
}

/// At live start: write HER's rows back, then check the whole profile once, so a gap is reported now.
pub(crate) fn startup(context: &RuntimeContext) {
    restore_for_start(context);
    let _ = admit(context);
}

/// Before DSH starts (and at live start): write HER's rows back in the install root's runtime; a failure is recorded
/// and prompts stay refused by `admit`.
pub(crate) fn restore_for_start(context: &RuntimeContext) {
    if !applies(context) {
        return;
    }
    match restore(context) {
        Ok(true) => record(context, Trigger::Startup, "profile-restore", Outcome::Ok, "the guarded profile rows were written back"),
        Ok(false) => {}
        Err(error) => record(context, Trigger::Startup, "profile-restore", Outcome::Failed, &error),
    }
}

/// Write HER's block into the profile's patch layer. `Ok(false)`: already current, nothing written. A changed file is
/// first saved as `cordis.patch.yml.bak-her-<UTC>` (newest five of those kept; other backups are never touched).
pub(crate) fn restore(context: &RuntimeContext) -> Result<bool, String> {
    let _lock = profile_lock(context)?;
    // The DSH home itself may not exist yet on a fresh install (the supervisor creates it before DSH starts too).
    context.resolve_path(RuntimeArea::DshUser, "", PathIntent::CreateDirectory).map_err(str::to_string)?;
    let existing = read_profile(context)?;
    let updated = apply_rows(existing.as_deref().unwrap_or(""))?;
    let current = existing.as_deref().map(|text| text.strip_prefix('\u{feff}').unwrap_or(text));
    if current == Some(updated.as_str()) {
        return Ok(false);
    }
    if let Some(old) = existing.as_deref() {
        backup(context, old)?;
    }
    // An edit made by hand since the read wins: nothing is written over it, and the next start tries again.
    if read_profile(context)? != existing {
        return Err("the profile changed during the restore; nothing was written".to_string());
    }
    context.write_atomic(RuntimeArea::DshUser, PROFILE_FILE, updated.as_bytes())?;
    Ok(true)
}

fn read_profile(context: &RuntimeContext) -> Result<Option<String>, String> {
    read_capped(context, PROFILE_FILE).map_err(|error| format!("the profile file cannot be read ({error}); nothing was written"))
}

/// One restore at a time per runtime (the supervisor and the live bridge may both start one).
fn profile_lock(context: &RuntimeContext) -> Result<std::fs::File, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match context.open_locked_file(RuntimeArea::Data, LOCK_FILE) {
            Ok(file) => return Ok(file),
            Err(error) if error == "runtime file is already locked" && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

fn backup(context: &RuntimeContext, old: &str) -> Result<(), String> {
    let name = format!("{BACKUP_PREFIX}{}", crate::dsh_mcp_config::utc_stamp());
    context.write_atomic(RuntimeArea::DshUser, &format!("{PROFILE_DIR}/{name}"), old.as_bytes())?;
    let dir = context
        .resolve_path(RuntimeArea::DshUser, PROFILE_DIR, PathIntent::ReadDirectory)
        .map_err(str::to_string)?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|_| "the profile folder cannot be listed".to_string())?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with(BACKUP_PREFIX))
        .collect();
    names.sort();
    for stale in &names[..names.len().saturating_sub(BACKUPS_KEPT)] {
        let _ = context.remove_file(RuntimeArea::DshUser, &format!("{PROFILE_DIR}/{stale}"));
    }
    Ok(())
}

fn read_capped(context: &RuntimeContext, relative: &str) -> Result<Option<String>, String> {
    let len = context.optional_file_len(RuntimeArea::DshUser, relative).map_err(str::to_string)?;
    if len > MAX_FILE_BYTES {
        return Err("the file is too large".to_string());
    }
    context.read_optional_text(RuntimeArea::DshUser, relative).map_err(str::to_string)
}

/// Every problem of the profile and the preset bundle, as sorted codes (`<row>:<file>`); `Ok` when there is none.
pub(crate) fn check(context: &RuntimeContext) -> Result<(), Vec<String>> {
    let mut problems = BTreeSet::new();
    check_profile(context, &mut problems);
    check_presets(context, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.into_iter().collect())
    }
}

fn check_profile(context: &RuntimeContext, problems: &mut BTreeSet<String>) {
    let Ok(Some(raw)) = read_capped(context, PROFILE_FILE) else {
        problems.insert("profile-file".to_string());
        return;
    };
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw.as_str());
    let lines: Vec<&str> = text.split('\n').map(|line| line.trim_end_matches('\r')).collect();
    let at = |marker: &str| -> Vec<usize> { (0..lines.len()).filter(|i| lines[*i] == marker).collect() };
    let (begins, ends) = (at(BEGIN), at(END));
    let ([begin], [end]) = (begins.as_slice(), ends.as_slice()) else {
        problems.insert("profile-file".to_string());
        return;
    };
    let (begin, end) = (*begin, *end);
    let block = render_block();
    let expected: Vec<&str> = block.trim_end_matches('\n').split('\n').collect();
    if begin >= end || lines[begin..=end] != expected[..] {
        problems.insert("profile-file".to_string());
        return;
    }
    if lines.iter().enumerate().any(|(index, line)| (index < begin || index > end) && matches!(*line, "[]" | "{}")) {
        problems.insert("profile-file".to_string());
    }
    scan(text, "profile", Some((begin, end)), problems);
}

fn check_presets(context: &RuntimeContext, problems: &mut BTreeSet<String>) {
    let manifest = match read_capped(context, &format!("{PRESET_BUNDLE}/package.json")) {
        Ok(Some(text)) => text,
        _ => {
            problems.insert("preset-bundle".to_string());
            return;
        }
    };
    let files: Vec<String> = match serde_json::from_str::<Value>(manifest.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|manifest| manifest.pointer("/dsh/bundle/patch").cloned())
    {
        Some(Value::String(one)) => vec![one],
        Some(Value::Array(list)) if list.iter().all(Value::is_string) => {
            list.iter().filter_map(|file| file.as_str().map(str::to_string)).collect()
        }
        _ => Vec::new(),
    };
    if files.is_empty() || files.len() > MAX_PRESET_FILES {
        problems.insert("preset-bundle".to_string());
        return;
    }
    for file in &files {
        let relative = file.strip_prefix("./").unwrap_or(file.as_str());
        let label = preset_label(relative);
        let safe = !relative.is_empty()
            && !relative.starts_with(['/', '\\'])
            && !relative.contains(':')
            && relative.split(['/', '\\']).all(|part| !part.is_empty() && part != "." && part != "..");
        let text = if safe {
            read_capped(context, &format!("{PRESET_BUNDLE}/{relative}")).ok().flatten()
        } else {
            None
        };
        match text {
            Some(text) => scan(text.strip_prefix('\u{feff}').unwrap_or(text.as_str()), &label, None, problems),
            None => {
                problems.insert(format!("preset-bundle:{label}"));
            }
        }
    }
}

/// The preset file's stem, limited to plain characters (it is shown to the owner).
fn preset_label(relative: &str) -> String {
    let leaf = relative.rsplit(['/', '\\']).next().unwrap_or(relative);
    let stem = leaf.strip_suffix(".patch.yml").unwrap_or(leaf);
    stem.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(40)
        .collect()
}

/// Problems of the rows in `text`. `owned` is HER's block (first and last line), left out of the check; a top-level
/// row above it is overridden by it and is not checked either.
fn scan(text: &str, label: &str, owned: Option<(usize, usize)>, problems: &mut BTreeSet<String>) {
    let outside = |line: usize| owned.is_none_or(|(begin, end)| line < begin || line > end);
    let entries = match entries(text) {
        Ok(entries) => entries,
        Err(_) => {
            problems.insert(format!("yaml:{label}"));
            return;
        }
    };
    let found: Vec<Item> = items(&entries).into_iter().filter(|item| outside(item.line)).collect();
    for (code, packages) in GUARDED_PACKAGES {
        for package in packages.iter() {
            let named = text
                .split('\n')
                .enumerate()
                .filter(|(line, raw)| outside(*line) && !raw.trim_start().starts_with('#') && names(raw, package))
                .count();
            let read = found.iter().filter(|item| is_package(&item.name, package)).count();
            if named != read {
                problems.insert(format!("{code}:{label}"));
            }
        }
    }
    for item in &found {
        if owned.is_some_and(|(begin, _)| item.col == 2 && item.line < begin) {
            continue;
        }
        if let Some(code) = row_problem(item) {
            problems.insert(format!("{code}:{label}"));
        }
        if (item.id == "persona" || is_package(&item.name, PERSONA_PACKAGE))
            && !crate::upstream_capture::denied_terms(&item.text).is_empty()
        {
            problems.insert(format!("persona-text:{label}"));
        }
    }
}

/// Whether `line` names `package` itself; a longer package name (`…-fs-search` for `…-fs`) does not count.
fn names(line: &str, package: &str) -> bool {
    line.match_indices(package).any(|(at, _)| {
        !line[at + package.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// `name` is `package` or one of its entry points (`package/sub`).
fn is_package(name: &str, package: &str) -> bool {
    name == package || name.strip_prefix(package).is_some_and(|rest| rest.starts_with('/'))
}

fn named(item: &Item, packages: &[&str]) -> bool {
    packages.iter().any(|package| is_package(&item.name, package))
}

fn row_problem(item: &Item) -> Option<&'static str> {
    let disabled = matches!(item.disabled.as_str(), "true" | "True" | "TRUE");
    if TOOL_IDS.contains(&item.id.as_str()) || named(item, TOOL_PACKAGES) {
        return (!disabled).then_some("local-tools");
    }
    if item.id == "harness-dev-context" || named(item, &[DEV_CONTEXT_PACKAGE]) {
        return (!disabled).then_some("dev-context");
    }
    if item.id == "system-prompt" || named(item, &[SYSTEM_PROMPT_PACKAGE]) {
        return (!disabled).then_some("system-prompt");
    }
    if item.id == "agent-instructions" || named(item, &[AGENT_INSTRUCTIONS_PACKAGE]) {
        return (!disabled && item.max_bytes.as_deref() != Some("0")).then_some("agent-instructions");
    }
    None
}

/// One mapping entry or sequence item of the line reader.
#[derive(Debug, Default)]
struct Entry {
    line: usize,
    /// Column of the key (for an item line: of the text after `- `).
    col: usize,
    item: bool,
    key: String,
    value: String,
    /// The lines of a block scalar, a quoted scalar or a plain scalar that go on below the entry.
    body: Vec<String>,
}

/// One sequence item (a loader row) with the keys the guard reads.
#[derive(Debug, Default)]
struct Item {
    line: usize,
    col: usize,
    id: String,
    name: String,
    disabled: String,
    max_bytes: Option<String>,
    /// The persona text of a `persona` row (`config.prefix`, `config.suffix`).
    text: String,
}

fn entries(text: &str) -> Result<Vec<Entry>, &'static str> {
    let mut out: Vec<Entry> = Vec::new();
    // Lines that belong to the last entry's scalar: a block scalar (deeper than its key) or an open quoted one.
    let mut block: Option<usize> = None;
    let mut quoted: Option<char> = None;
    for (index, raw) in text.split('\n').enumerate() {
        let line = raw.trim_end_matches('\r');
        let content = line.trim_start_matches(' ');
        let indent = line.len() - content.len();
        if let Some(quote) = quoted {
            if let Some(last) = out.last_mut() {
                last.body.push(content.to_string());
            }
            if quote_closes(&format!(" {content}"), quote) {
                quoted = None;
            }
            continue;
        }
        if let Some(column) = block {
            if content.is_empty() || indent > column {
                if let Some(last) = out.last_mut() {
                    last.body.push(content.to_string());
                }
                continue;
            }
            block = None;
        }
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        if content.starts_with('\t') {
            return Err("tab indentation");
        }
        if content == "---" || content == "..." || content.starts_with('%') {
            return Err("document marker");
        }
        let (item, col, body) = match content.strip_prefix("- ") {
            Some(rest) => {
                let trimmed = rest.trim_start_matches(' ');
                (true, indent + 2 + rest.len() - trimmed.len(), trimmed)
            }
            None if content == "-" => return Err("empty sequence item"),
            None => (false, indent, content),
        };
        if body.starts_with("- ") || body == "-" {
            return Err("nested sequence");
        }
        if body.starts_with('{') || body.starts_with('[') {
            if body == "{}" || body == "[]" {
                out.push(Entry { line: index, col, item, value: body.to_string(), ..Entry::default() });
                continue;
            }
            return Err("flow collection");
        }
        let Some((key, mut value)) = split_key(body) else {
            // A scalar list item, or the next line of a multi-line plain scalar.
            if item {
                out.push(Entry { line: index, col, item, value: body.to_string(), ..Entry::default() });
            } else if let Some(last) = out.last_mut() {
                last.body.push(body.to_string());
            }
            continue;
        };
        if value.starts_with('#') {
            value.clear();
        } else if !value.starts_with(['\'', '"']) {
            if let Some(at) = value.find(" #") {
                value = value[..at].trim_end().to_string();
            }
        }
        if key == "<<" || value.starts_with('&') || value.starts_with('*') {
            return Err("anchor or alias");
        }
        if (value.starts_with('{') && value != "{}") || (value.starts_with('[') && value != "[]") {
            return Err("flow collection");
        }
        if value.starts_with('|') || value.starts_with('>') {
            block = Some(col);
        } else if let Some(quote) = value.chars().next().filter(|c| *c == '\'' || *c == '"') {
            if !quote_closes(&value, quote) {
                quoted = Some(quote);
            }
        }
        out.push(Entry { line: index, col, item, key, value, body: Vec::new() });
    }
    Ok(out)
}

/// `key: value` or `key:` (the key plain or quoted); `None` for a line that is no mapping entry.
fn split_key(content: &str) -> Option<(String, String)> {
    if let Some(quote) = content.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let close = content[1..].find(quote)? + 1;
        let rest = content[close + 1..].strip_prefix(':')?;
        if !rest.is_empty() && !rest.starts_with(' ') {
            return None;
        }
        return Some((content[1..close].to_string(), rest.trim().to_string()));
    }
    let at = content.match_indices(':').map(|(at, _)| at).find(|at| {
        let after = &content[*at + 1..];
        after.is_empty() || after.starts_with(' ')
    })?;
    Some((content[..at].trim_end().to_string(), content[at + 1..].trim().to_string()))
}

/// Whether the quoted scalar opened by the first character of `value` closes on this line.
fn quote_closes(value: &str, quote: char) -> bool {
    let mut chars = value.chars().skip(1).peekable();
    while let Some(c) = chars.next() {
        if quote == '"' && c == '\\' {
            chars.next();
            continue;
        }
        if c == quote {
            if quote == '\'' && chars.peek() == Some(&'\'') {
                chars.next();
                continue;
            }
            return true;
        }
    }
    false
}

/// A plain or quoted scalar without its quotes or a trailing comment.
fn unquote(value: &str) -> String {
    let value = value.trim();
    if let Some(quote) = value.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let inner = &value[1..];
        return match inner.find(quote) {
            Some(end) => inner[..end].to_string(),
            None => inner.to_string(),
        };
    }
    match value.find(" #") {
        Some(at) => value[..at].trim_end().to_string(),
        None => value.to_string(),
    }
}

fn items(entries: &[Entry]) -> Vec<Item> {
    let mut out = Vec::new();
    for (start, first) in entries.iter().enumerate().filter(|(_, entry)| entry.item) {
        let col = first.col;
        let mut item = Item { line: first.line, col, ..Item::default() };
        let mut index = start;
        loop {
            let entry = &entries[index];
            match entry.key.as_str() {
                "id" => item.id = unquote(&entry.value),
                "name" => item.name = unquote(&entry.value),
                "disabled" => item.disabled = unquote(&entry.value),
                "config" => {
                    let children: Vec<&Entry> =
                        entries[index + 1..].iter().take_while(|child| child.col > col).collect();
                    if let Some(depth) = children.first().map(|child| child.col) {
                        for child in children.iter().filter(|child| child.col == depth && !child.item) {
                            match child.key.as_str() {
                                "maxBytes" => item.max_bytes = Some(unquote(&child.value)),
                                "prefix" | "suffix" => {
                                    if !child.value.starts_with('|') && !child.value.starts_with('>') {
                                        item.text.push_str(&unquote(&child.value));
                                        item.text.push('\n');
                                    }
                                    for line in &child.body {
                                        item.text.push_str(line);
                                        item.text.push('\n');
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            index += 1;
            while index < entries.len() && entries[index].col > col {
                index += 1;
            }
            if index >= entries.len() || entries[index].col < col || entries[index].item {
                break;
            }
        }
        out.push(item);
    }
    out
}

/// `her-cli dsh-profile check --runtime-root <root>`: read-only, whether or not the root is the install root's.
/// Exit 0 and `ok` when every guarded row is in place, 1 and one problem code per line otherwise, 2 on usage or
/// runtime errors.
pub(crate) fn run_cli(args: &[String]) -> i32 {
    let [command, flag, root] = args else {
        eprintln!("{USAGE}");
        return 2;
    };
    if command != "check" || flag != "--runtime-root" {
        eprintln!("{USAGE}");
        return 2;
    }
    let context = match RuntimeContext::from_root(Path::new(root)) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("dsh-profile: {error}");
            return 2;
        }
    };
    match check(&context) {
        Ok(()) => {
            println!("ok");
            0
        }
        Err(problems) => {
            println!("{}", problems.join("\n"));
            1
        }
    }
}

/// Fixtures the guard tests here and in `pipeline` share.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::runtime::RuntimeContext;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Runtime roots the tests guard as if they were the install root's runtime.
    pub(crate) static FORCED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
    /// Owner alerts the guard raised: (runtime scope, text). Tests never send them.
    pub(crate) static ALERTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    pub(crate) const PRESET_FILE: &str = "bundles/qq-agent-presets/presets/fixture-chat.patch.yml";
    pub(crate) const MANIFEST: &str =
        r#"{"name":"qq-agent-presets","dsh":{"bundle":{"patch":["./presets/fixture-chat.patch.yml"]}}}"#;
    /// A synthetic profile patch layer: a hand override above HER's block and an MCP insert.
    pub(crate) const PROFILE_HAND: &str = concat!(
        "# synthetic profile patch layer\n",
        "- id: system-prompt\n",
        "  config:\n",
        "    includeHarnessIdentity: true\n",
        "- insert:\n",
        "    - id: mcp-fixture\n",
        "      name: '@deepseek-ai/dsh-mcp-client'\n",
        "      config:\n",
        "        serverName: fixture\n",
        "        args:\n",
        "          - '--mcp-serve'\n",
    );
    /// A synthetic preset in the shape the preset generator writes, with every guarded row in place.
    pub(crate) const PRESET: &str = concat!(
        "# synthetic preset fixture\n",
        "- insert:\n",
        "    - id: preset-fixture-chat\n",
        "      name: '@deepseek-ai/dsh-agent-preset'\n",
        "      config:\n",
        "        id: fixture-chat\n",
        "        name: Fixture chat\n",
        "        order: 1\n",
        "        plugins:\n",
        "          - id: persona\n",
        "            name: '@deepseek-ai/dsh-persona'\n",
        "            config:\n",
        "              prefix: |-\n",
        "                你是群里的普通成员。\n",
        "                - 不用列表回答。\n",
        "                说话简短。\n",
        "          - id: agent-instructions\n",
        "            name: '@deepseek-ai/dsh-agent-instructions'\n",
        "            config:\n",
        "              maxBytes: 0\n",
        "          - id: tool-pwsh\n",
        "            name: '@deepseek-ai/dsh-tool-pwsh'\n",
        "            disabled: true\n",
        "          - id: tool-fs\n",
        "            name: '@deepseek-ai/dsh-tool-fs'\n",
        "            disabled: true\n",
        "          - id: tool-fs-search\n",
        "            name: '@deepseek-ai/dsh-tool-fs-search'\n",
        "            disabled: true\n",
        "            config:\n",
        "              sampleOverCapGlobResults: false\n",
        "          - id: tool-web\n",
        "            name: '@deepseek-ai/dsh-tool-web'\n",
        "            config:\n",
        "              search: false\n",
        "              fetch: false\n",
        "          - id: qq-tool-restrict\n",
        "            name: qq-agent-presets/qq-tool-restrict.mjs\n",
        "            config: {}\n",
    );
    /// The edit that turns the preset's `pwsh` tool back on.
    pub(crate) const PWSH_ON: (&str, &str) = (
        "            name: '@deepseek-ai/dsh-tool-pwsh'\n            disabled: true\n",
        "            name: '@deepseek-ai/dsh-tool-pwsh'\n",
    );

    /// A disposable selected runtime, removed on drop.
    pub(crate) struct Root(pub(crate) PathBuf);

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn fixture(label: &str) -> (Root, RuntimeContext) {
        let root = crate::install_root::development_root(
            std::env::temp_dir().join(format!("her-179-{label}-{}", crate::turn_ledger::mint_id(""))),
        );
        for dir in ["config", "data", "resources"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("config/config.json"), r#"{"consoleToken":"synthetic-console-token-179","ownerQQ":8808}"#)
            .unwrap();
        std::fs::write(root.join("data/social-v2.json"), "{}").unwrap();
        std::fs::write(
            root.join("dependencies.json"),
            r#"{"dsh":"http://127.0.0.1:9","model":"http://127.0.0.1:9","onebot":"http://127.0.0.1:9"}"#,
        )
        .unwrap();
        let context = RuntimeContext::from_root(&root).unwrap();
        (Root(root), context)
    }

    /// Guard this runtime as the install root's runtime.
    pub(crate) fn force(context: &RuntimeContext) {
        FORCED.lock().unwrap_or_else(|p| p.into_inner()).insert(context.runtime_root().unwrap().to_path_buf());
    }

    /// The owner alerts raised for this runtime, oldest first.
    pub(crate) fn alerts(context: &RuntimeContext) -> Vec<String> {
        let scope = super::scope(context);
        ALERTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(of, _)| *of == scope)
            .map(|(_, text)| text.clone())
            .collect()
    }

    /// The runtime's DSH home.
    pub(crate) fn home(context: &RuntimeContext) -> PathBuf {
        context.runtime_root().unwrap().join("user/dsh")
    }

    pub(crate) fn write(context: &RuntimeContext, relative: &str, text: &str) {
        let path = home(context).join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A profile with HER's block last, a preset bundle listing one preset and that preset, all passing the check.
    pub(crate) fn write_compliant(context: &RuntimeContext) {
        write(context, &format!("{}/package.json", super::PRESET_BUNDLE), MANIFEST);
        write(context, PRESET_FILE, PRESET);
        write(context, super::PROFILE_FILE, &format!("{PROFILE_HAND}{}", super::render_block()));
    }

    /// Rewrite the fixture preset; the edit must change it.
    pub(crate) fn edit_preset(context: &RuntimeContext, edit: impl Fn(&str) -> String) {
        let path = home(context).join(PRESET_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        let next = edit(&text);
        assert_ne!(next, text, "the edit must change the fixture preset");
        std::fs::write(path, next).unwrap();
    }
}

#[cfg(test)]
#[path = "dsh_profile_tests.rs"]
mod tests;
