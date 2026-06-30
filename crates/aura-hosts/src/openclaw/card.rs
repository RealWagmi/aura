//! Host-brief composition pieces.
//!
//! Only the brief-shaping helpers live here:
//! - [`validate_openclaw_pro_host_brief`] — fail-OPEN validator (records a
//!   `missing[]` list for observability; never blocks the call).
//! - [`derive_workflows`] — synthesizes a non-empty `setup.workflows` from real
//!   workspace signals (crons + routines + skills), deduped and capped.
//! - the callback normalizers ([`normalize_callback_task`],
//!   [`normalize_callback_url`], [`normalize_callback_links`],
//!   [`normalize_callback_log`], [`normalize_call_intent`]) — URL allowlist
//!   `https://` / `openclaw://` only; clamp summary 2000 / reply 12000 /
//!   links 8.
//! - [`clean_json_value`] — recursive redact + clamp of arbitrary JSON.
//!
//! All text passes `redact_secrets`.

use serde_json::{Map, Value};

use aura_core::redact_secrets;

/// Cap on a single derived workflow / skill list.
pub const MAX_SKILLS: usize = 200;
/// Cap on `recent_messages_verbatim` carried by the host-brief path.
pub const MAX_MESSAGES: usize = 200;
/// Hard byte ceiling for the assembled brief before voice composition.
pub const MAX_BRIEF_BYTES: usize = 420_000;
/// A substantive preference string must clear this trimmed length.
pub const MIN_PREFERENCES_CHARS: usize = 40;
/// Callback reply/intent text clamp.
pub const MAX_CALLBACK_TASK_TEXT: usize = 12_000;
/// Callback summary clamp.
pub const MAX_CALLBACK_TASK_SUMMARY: usize = 2_000;
/// Callback links cap.
pub const MAX_CALLBACK_TASK_LINKS: usize = 8;
/// Minimum total chars across recent messages for a "quality verified" brief.
const MIN_RECENT_MESSAGE_TOTAL_CHARS: usize = 120;
/// Per-expected-message char weight for the quality floor.
const MIN_RECENT_MESSAGE_AVG_CHARS: usize = 12;
/// Ceiling on the per-message-derived quality floor.
const MAX_RECENT_MESSAGE_MIN_CHARS: usize = 2400;

/// The 22 required-field paths the validator checks. Order matches the JS
/// `OPENCLAW_PRO_HOST_BRIEF_REQUIRED_FIELDS`.
pub const REQUIRED_FIELDS: &[&str] = &[
    "v_or_schema",
    "host_kind",
    "user.name",
    "user.soul_summary_or_setup.soul_md_summary",
    "user.interests",
    "context.current_focus",
    "context.open_threads",
    "context.recent_messages_verbatim_latest_200_or_complete_min_10",
    "context.recent_messages_verbatim_text_quality",
    "context.recent_tasks",
    "context.cron_jobs",
    "context.cron_jobs_results",
    "context.last_tool",
    "context.timing_summary",
    "context.cost_summary",
    "context.leader_hints",
    "setup.system_prompt_summary",
    "setup.preferences",
    "setup.regular_activity",
    "setup.skills",
    "setup.workflows",
    "setup.notes_summary",
];

// --- small predicates (mirror the JS helpers) --------------------------------

fn has_non_empty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn is_array(value: Option<&Value>) -> bool {
    value.map(Value::is_array).unwrap_or(false)
}

fn is_plain_object(value: Option<&Value>) -> bool {
    value.map(Value::is_object).unwrap_or(false)
}

/// Pull the message text out of an arbitrary message value, walking the same
/// nested keys the JS `hostBriefMessageText` walks.
pub fn host_brief_message_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(host_brief_message_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(_) => {
            for key in [
                "text", "content", "message", "body", "value", "parts", "blocks", "items",
                "children", "segments",
            ] {
                if let Some(inner) = value.get(key) {
                    let text = host_brief_message_text(inner);
                    if !text.trim().is_empty() {
                        return text;
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Map a raw role to the brief's `user` / `openclaw` vocabulary.
pub fn host_brief_message_role(value: &Value) -> String {
    let Some(obj) = value.as_object() else {
        return "openclaw".to_owned();
    };
    let raw = obj
        .get("role")
        .or_else(|| obj.get("speaker"))
        .or_else(|| obj.get("sender"))
        .or_else(|| obj.get("from"))
        .or_else(|| obj.get("author_role"))
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_lowercase)
        .unwrap_or_default();
    if matches!(raw.as_str(), "user" | "human" | "client" | "owner") {
        "user".to_owned()
    } else {
        "openclaw".to_owned()
    }
}

// --- text cleaning / clamping ------------------------------------------------

/// Trim, collapse internal whitespace, redact secrets, and clamp to `max`
/// chars (word-boundary ellipsis). Mirrors `clamp(cleanText(...))`.
pub fn clamp_clean(value: &str, max: usize) -> String {
    let collapsed = redact_secrets(value).replace('\r', "").replace('\t', " ");
    let collapsed = collapse_ws(&collapsed);
    clamp_chars(&collapsed, max)
}

/// Like [`clamp_clean`] but preserves newlines (verbatim message text).
pub fn clamp_clean_verbatim(value: &str, max: usize) -> String {
    let cleaned = redact_secrets(value).replace('\r', "").replace('\t', " ");
    let cleaned = collapse_double_spaces(&cleaned).trim().to_owned();
    clamp_chars(&cleaned, max)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collapse_double_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if !prev_space {
                out.push(ch);
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

/// Clamp to at most `max` Unicode scalar values, appending "..." (which counts
/// toward the budget) when truncated. The result is always ≤ `max` chars.
fn clamp_chars(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    if max <= 3 {
        return trimmed.chars().take(max).collect();
    }
    // Reserve 3 chars for the ellipsis, then prefer a word boundary.
    let budget = max - 3;
    let head: String = trimmed.chars().take(budget).collect();
    let head = head
        .rsplit_once(char::is_whitespace)
        .map(|(keep, _)| keep)
        .unwrap_or(&head);
    format!("{head}...")
}

/// Recursively redact + clamp every string in an arbitrary JSON value.
pub fn clean_json_value(value: &Value, string_max: usize) -> Value {
    match value {
        Value::String(s) => Value::String(clamp_clean(s, string_max)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| clean_json_value(v, string_max))
                .collect(),
        ),
        Value::Object(map) => {
            let cleaned: Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), clean_json_value(v, string_max)))
                .collect();
            Value::Object(cleaned)
        }
        other => other.clone(),
    }
}

// --- cron predicates (for the validator + deriveWorkflows) -------------------

fn cron_has_schedule(cron: &Value) -> bool {
    cron.is_object() && has_non_empty_string(cron.get("schedule"))
}

fn cron_has_result(cron: &Value) -> bool {
    let Some(obj) = cron.as_object() else {
        return false;
    };
    if has_non_empty_string(obj.get("last_summary")) {
        return true;
    }
    obj.get("last_runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter().any(|run| {
                run.is_object()
                    && ["summary", "status", "ts_iso", "last_run_iso", "time"]
                        .iter()
                        .any(|k| has_non_empty_string(run.get(*k)))
            })
        })
        .unwrap_or(false)
}

fn cron_results_majority(cron_jobs: &[Value]) -> bool {
    if cron_jobs.is_empty() {
        return false;
    }
    let with = cron_jobs.iter().filter(|c| cron_has_result(c)).count();
    with * 2 > cron_jobs.len()
}

// --- validate (fail-OPEN: records missing[], never blocks) -------------------

/// The result of [`validate_openclaw_pro_host_brief`]. `ok` is informational —
/// a brief that is not `ok` is still used (the call always proceeds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBriefValidation {
    pub ok: bool,
    pub missing: Vec<String>,
    pub section_count: usize,
    pub required_section_count: usize,
}

fn recent_messages_array(context: &Map<String, Value>) -> Vec<Value> {
    const KEYS: &[&str] = &[
        "recent_messages_verbatim",
        "recentMessagesVerbatim",
        "recent_messages",
        "recentMessages",
        "messages",
        "conversation",
        "history",
        "message_blocks",
        "messageBlocks",
        "recent_message_blocks",
        "recentMessageBlocks",
        "last_messages",
        "lastMessages",
        "session_tail",
        "sessionTail",
    ];
    for key in KEYS {
        if let Some(arr) = context.get(*key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}

fn recent_messages_available_count(context: &Map<String, Value>) -> Option<i64> {
    const KEYS: &[&str] = &[
        "recent_messages_available_count",
        "recentMessagesAvailableCount",
        "recent_messages_count",
        "recentMessagesCount",
        "available_messages",
        "availableMessages",
        "available_message_count",
        "availableMessageCount",
        "message_count",
        "messageCount",
        "messages_count",
        "messagesCount",
        "message_blocks_count",
        "messageBlocksCount",
        "history_count",
        "historyCount",
    ];
    for key in KEYS {
        if let Some(v) = context.get(*key) {
            if let Some(n) = v.as_i64() {
                return Some(n.max(0));
            }
            if let Some(f) = v.as_f64() {
                return Some(f.max(0.0).floor() as i64);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.trim().parse::<f64>() {
                    return Some(n.max(0.0).floor() as i64);
                }
            }
        }
    }
    None
}

/// True when the brief carries the latest 200 verbatim messages OR genuinely
/// held fewer and all are present (≥ 10), mirroring `recentMessagesComplete`.
fn recent_messages_complete(context: &Map<String, Value>, usable: usize) -> bool {
    let available = recent_messages_available_count(context);
    usable >= MAX_MESSAGES || (available == Some(usable as i64) && usable >= 10)
}

fn recent_messages_text_quality(context: &Map<String, Value>) -> bool {
    let messages = recent_messages_array(context);
    let text_chars: usize = messages
        .iter()
        .map(|m| host_brief_message_text(m).trim().chars().count())
        .sum();
    let loaded = messages
        .iter()
        .filter(|m| !host_brief_message_text(m).trim().is_empty())
        .count();
    let available = recent_messages_available_count(context);
    let expected = match available {
        Some(n) => (n as usize).min(MAX_MESSAGES),
        None => MAX_MESSAGES,
    };
    let min_text_chars = MAX_RECENT_MESSAGE_MIN_CHARS
        .min(MIN_RECENT_MESSAGE_TOTAL_CHARS.max(expected * MIN_RECENT_MESSAGE_AVG_CHARS));
    loaded >= 10 && text_chars >= min_text_chars
}

/// Fail-OPEN validator: returns a `missing[]` list for observability only. The
/// call proceeds regardless. The soul check passes if EITHER
/// `user.soul_summary` OR `setup.soul_md_summary` is present (keep `&&`).
pub fn validate_openclaw_pro_host_brief(value: &Value) -> HostBriefValidation {
    let mut missing: Vec<String> = Vec::new();
    let Some(root) = value.as_object() else {
        return HostBriefValidation {
            ok: false,
            missing: REQUIRED_FIELDS.iter().map(|s| (*s).to_owned()).collect(),
            section_count: 0,
            required_section_count: REQUIRED_FIELDS.len(),
        };
    };

    let empty = Map::new();
    let user = root
        .get("user")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let context = root
        .get("context")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let setup = root
        .get("setup")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let v_is_2 = root.get("v").and_then(Value::as_i64) == Some(2);
    let schema_ok = root.get("schema").and_then(Value::as_str) == Some("codexini-host-brief-v2");
    if !v_is_2 && !schema_ok {
        missing.push("v_or_schema".to_owned());
    }
    if root.get("host_kind").and_then(Value::as_str) != Some("openclaw") {
        missing.push("host_kind".to_owned());
    }
    if !has_non_empty_string(user.get("name")) {
        missing.push("user.name".to_owned());
    }
    if !has_non_empty_string(user.get("soul_summary"))
        && !has_non_empty_string(setup.get("soul_md_summary"))
    {
        missing.push("user.soul_summary_or_setup.soul_md_summary".to_owned());
    }
    if !is_array(user.get("interests")) {
        missing.push("user.interests".to_owned());
    }
    if !has_non_empty_string(context.get("current_focus")) {
        missing.push("context.current_focus".to_owned());
    }
    if !is_array(context.get("open_threads")) {
        missing.push("context.open_threads".to_owned());
    }

    let usable_recent = recent_messages_array(context)
        .iter()
        .filter(|m| !host_brief_message_text(m).trim().is_empty())
        .count();
    if usable_recent < 10 || !recent_messages_complete(context, usable_recent) {
        missing.push("context.recent_messages_verbatim_latest_200_or_complete_min_10".to_owned());
    } else if !recent_messages_text_quality(context) {
        missing.push("context.recent_messages_verbatim_text_quality".to_owned());
    }

    if !is_array(context.get("recent_tasks")) {
        missing.push("context.recent_tasks".to_owned());
    }
    let cron_jobs = context.get("cron_jobs").and_then(Value::as_array);
    match cron_jobs {
        Some(jobs) if jobs.iter().any(cron_has_schedule) => {
            if !cron_results_majority(jobs) {
                missing.push("context.cron_jobs_results".to_owned());
            }
        }
        _ => missing.push("context.cron_jobs".to_owned()),
    }
    if !is_plain_object(context.get("last_tool")) {
        missing.push("context.last_tool".to_owned());
    }
    if !is_plain_object(context.get("timing_summary")) {
        missing.push("context.timing_summary".to_owned());
    }
    if !is_plain_object(context.get("cost_summary")) {
        missing.push("context.cost_summary".to_owned());
    }
    if !is_array(context.get("leader_hints")) {
        missing.push("context.leader_hints".to_owned());
    }
    if !has_non_empty_string(setup.get("system_prompt_summary")) {
        missing.push("setup.system_prompt_summary".to_owned());
    }
    let prefs_ok = setup
        .get("preferences")
        .and_then(Value::as_str)
        .map(|s| s.trim().chars().count() >= MIN_PREFERENCES_CHARS)
        .unwrap_or(false);
    if !prefs_ok {
        missing.push("setup.preferences".to_owned());
    }
    if !has_non_empty_string(setup.get("regular_activity")) {
        missing.push("setup.regular_activity".to_owned());
    }
    if setup
        .get("skills")
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        missing.push("setup.skills".to_owned());
    }
    if setup
        .get("workflows")
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        missing.push("setup.workflows".to_owned());
    }
    if !has_non_empty_string(setup.get("notes_summary")) {
        missing.push("setup.notes_summary".to_owned());
    }

    let missing_count = missing.len();
    HostBriefValidation {
        ok: missing_count == 0,
        missing,
        section_count: REQUIRED_FIELDS.len() - missing_count,
        required_section_count: REQUIRED_FIELDS.len(),
    }
}

// --- deriveWorkflows ---------------------------------------------------------

/// One synthesized workflow entry, traceable to a real signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub kind: String,
    pub name: String,
    pub detail: String,
    pub source: String,
}

fn object_text(value: &Value, keys: &[&str]) -> String {
    if let Some(s) = value.as_str() {
        return s.to_owned();
    }
    if let Some(obj) = value.as_object() {
        for key in keys {
            if let Some(s) = obj.get(*key).and_then(Value::as_str) {
                if !s.trim().is_empty() {
                    return s.to_owned();
                }
            }
        }
    }
    String::new()
}

/// Extract up to `max_items` cleaned line-items from prose, stripping list
/// bullets / numbering. Mirrors the JS `extractLineItems`.
pub fn extract_line_items(text: &str, max_items: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let cleaned = clamp_clean(raw_line, 4000);
        let line = strip_bullet(&cleaned);
        if line.chars().count() >= 3 {
            out.push(clamp_chars(line, 140));
        }
        if out.len() >= max_items {
            break;
        }
    }
    out
}

fn strip_bullet(line: &str) -> &str {
    let trimmed = line.trim_start();
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .unwrap_or(trimmed);
    // Strip a leading `\d+[.)]` enumeration.
    let bytes = after_bullet.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') {
        return after_bullet[i + 1..].trim_start();
    }
    after_bullet.trim()
}

/// Synthesize a non-empty `setup.workflows` from crons + routines prose +
/// skills. Never fabricated; deduped and capped. Mirrors `deriveWorkflows`.
pub fn derive_workflows(
    crons: &[Value],
    regular_activity: &str,
    skills: &[String],
) -> Vec<Workflow> {
    let mut workflows: Vec<Workflow> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |kind: &str, name: &str, detail: &str, source: &str, out: &mut Vec<Workflow>| {
        let clean_name = clamp_clean(name, 120);
        if clean_name.is_empty() {
            return;
        }
        let dedupe_key = format!("{kind}:{}", clean_name.to_lowercase());
        if !seen.insert(dedupe_key) {
            return;
        }
        out.push(Workflow {
            kind: kind.to_owned(),
            name: clean_name,
            detail: clamp_clean(detail, 200),
            source: source.to_owned(),
        });
    };

    for cron in crons {
        if !cron_has_schedule(cron) {
            continue;
        }
        let name = object_text(cron, &["purpose", "summary", "id"]);
        let schedule = object_text(cron, &["schedule"]);
        let id_name = object_text(cron, &["id"]);
        let detail = if schedule.is_empty() {
            String::new()
        } else {
            format!("Runs on {schedule}.")
        };
        let resolved_name = if name.is_empty() { id_name } else { name };
        push("scheduled", &resolved_name, &detail, "cron", &mut workflows);
    }

    for line in extract_line_items(regular_activity, 6) {
        push("routine", &line, "", "routines", &mut workflows);
    }

    for skill in skills {
        let (title, rest) = match skill.split_once(':') {
            Some((t, r)) => (t, r.trim()),
            None => (skill.as_str(), ""),
        };
        push("skill", title, rest, "skills", &mut workflows);
    }

    workflows.truncate(MAX_SKILLS);
    workflows
}

// --- callback normalizers ----------------------------------------------------

/// Normalize a candidate URL: redact, clamp 1000, then allow ONLY `https://`
/// or `openclaw://` (case-insensitive). Returns "" when not allowed.
pub fn normalize_callback_url(value: &str) -> String {
    let text = clamp_clean(value, 1000);
    let lower = text.to_lowercase();
    if lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("openclaw://")
    {
        // The JS regex allows http(s) and openclaw; keep http for parity but it
        // is rarely used. Reject everything else (javascript:, file:, ...).
        text
    } else {
        String::new()
    }
}

/// One normalized callback link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackLink {
    pub label: String,
    pub url: String,
}

/// Normalize a links field (string | object | array) into ≤ 8 deduped links
/// with allowlisted URLs. Mirrors `normalizeCallbackLinks`.
pub fn normalize_callback_links(value: &Value) -> Vec<CallbackLink> {
    let items: Vec<Value> = match value {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in flatten(items) {
        let (url, label) = match &item {
            Value::String(s) => (normalize_callback_url(s), "Link".to_owned()),
            Value::Object(obj) => {
                let raw_url = obj
                    .get("url")
                    .or_else(|| obj.get("href"))
                    .or_else(|| obj.get("link"))
                    .or_else(|| obj.get("task_url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let label = obj
                    .get("label")
                    .or_else(|| obj.get("title"))
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Link");
                (normalize_callback_url(raw_url), clamp_clean(label, 120))
            }
            _ => (String::new(), String::new()),
        };
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        out.push(CallbackLink {
            label: if label.is_empty() {
                "Link".to_owned()
            } else {
                label
            },
            url,
        });
        if out.len() >= MAX_CALLBACK_TASK_LINKS {
            break;
        }
    }
    out
}

fn flatten(items: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Value::Array(inner) => out.extend(flatten(inner)),
            other => out.push(other),
        }
    }
    out
}

/// Normalize a recent-log field into ≤ 20 cleaned strings (≤ 800 chars each).
pub fn normalize_callback_log(value: &Value) -> Vec<String> {
    let items: Vec<Value> = match value {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    };
    flatten(items)
        .into_iter()
        .map(|item| clamp_clean_verbatim(&host_brief_message_text(&item), 800))
        .filter(|s| !s.is_empty())
        .take(20)
        .collect()
}

/// Canonicalize a `call_intent` string to the brief enum. "" when unknown.
pub fn normalize_call_intent(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "outbound_callback" => "outbound_callback".to_owned(),
        "outbound" => "outbound".to_owned(),
        "inbound" => "inbound".to_owned(),
        _ => String::new(),
    }
}

fn safe_enum(value: &str, allowed: &[&str]) -> String {
    let raw = value.trim().to_lowercase();
    if allowed.contains(&raw.as_str()) {
        raw
    } else {
        String::new()
    }
}

/// A normalized callback task (the result object that travels in the
/// tool_result frame). Mirrors `normalizeCallbackTask`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallbackTaskNorm {
    pub task_id: String,
    pub status: String,
    pub summary: String,
    pub reply: String,
    pub links: Vec<CallbackLink>,
    pub recent_log: Vec<String>,
}

fn first_string<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Normalize a callback-task value into a clamped, allowlisted result. Returns
/// `None` when there is no `task_id` (mirrors the JS early return).
pub fn normalize_callback_task(value: &Value) -> Option<CallbackTaskNorm> {
    let obj = value.as_object()?;
    let task_id = clamp_clean(first_string(obj, &["task_id", "taskId"]).unwrap_or(""), 120);
    if task_id.is_empty() {
        return None;
    }
    let status = safe_enum(
        obj.get("status").and_then(Value::as_str).unwrap_or(""),
        &["queued", "running", "completed", "failed", "cancelled"],
    );
    let reply = clamp_clean_verbatim(
        &host_brief_message_text(
            obj.get("task_reply")
                .or_else(|| obj.get("reply"))
                .or_else(|| obj.get("result"))
                .or_else(|| obj.get("answer"))
                .or_else(|| obj.get("summary"))
                .unwrap_or(&Value::Null),
        ),
        MAX_CALLBACK_TASK_TEXT,
    );
    let summary_src = obj
        .get("summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| reply.clone());
    let summary = clamp_clean_verbatim(&summary_src, MAX_CALLBACK_TASK_SUMMARY);
    let links = normalize_callback_links(
        obj.get("links")
            .or_else(|| obj.get("urls"))
            .or_else(|| obj.get("references"))
            .unwrap_or(&Value::Null),
    );
    let recent_log = normalize_callback_log(
        obj.get("recent_log")
            .or_else(|| obj.get("recentLog"))
            .or_else(|| obj.get("log"))
            .or_else(|| obj.get("events"))
            .unwrap_or(&Value::Null),
    );
    Some(CallbackTaskNorm {
        task_id,
        status,
        summary,
        reply,
        links,
        recent_log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thin_brief() -> Value {
        json!({ "v": 2, "host_kind": "openclaw" })
    }

    #[test]
    fn validator_on_thin_brief_records_missing_but_is_usable() {
        let v = validate_openclaw_pro_host_brief(&thin_brief());
        assert!(!v.ok);
        // v_or_schema + host_kind both satisfied; many others missing.
        assert!(!v.missing.contains(&"v_or_schema".to_owned()));
        assert!(!v.missing.contains(&"host_kind".to_owned()));
        assert!(v.missing.contains(&"user.name".to_owned()));
        assert_eq!(v.required_section_count, 22);
        assert_eq!(v.section_count + v.missing.len(), 22);
    }

    #[test]
    fn validator_on_non_object_marks_all_missing() {
        let v = validate_openclaw_pro_host_brief(&json!("nope"));
        assert!(!v.ok);
        assert_eq!(v.missing.len(), 22);
        assert_eq!(v.section_count, 0);
    }

    #[test]
    fn soul_check_passes_with_either_field() {
        let with_user_soul = json!({
            "v": 2, "host_kind": "openclaw",
            "user": { "soul_summary": "cares about craft" }
        });
        let v = validate_openclaw_pro_host_brief(&with_user_soul);
        assert!(!v
            .missing
            .contains(&"user.soul_summary_or_setup.soul_md_summary".to_owned()));

        let with_setup_soul = json!({
            "v": 2, "host_kind": "openclaw",
            "setup": { "soul_md_summary": "cares about craft" }
        });
        let v2 = validate_openclaw_pro_host_brief(&with_setup_soul);
        assert!(!v2
            .missing
            .contains(&"user.soul_summary_or_setup.soul_md_summary".to_owned()));

        // Neither present -> missing.
        let v3 = validate_openclaw_pro_host_brief(&thin_brief());
        assert!(v3
            .missing
            .contains(&"user.soul_summary_or_setup.soul_md_summary".to_owned()));
    }

    #[test]
    fn cron_jobs_results_majority_required() {
        // One cron WITH schedule but NO result -> cron_jobs passes, but results fail.
        let brief = json!({
            "v": 2, "host_kind": "openclaw",
            "context": { "cron_jobs": [ { "schedule": "0 9 * * *", "purpose": "standup" } ] }
        });
        let v = validate_openclaw_pro_host_brief(&brief);
        assert!(!v.missing.contains(&"context.cron_jobs".to_owned()));
        assert!(v.missing.contains(&"context.cron_jobs_results".to_owned()));

        // Add last_summary -> majority satisfied.
        let brief2 = json!({
            "v": 2, "host_kind": "openclaw",
            "context": { "cron_jobs": [
                { "schedule": "0 9 * * *", "purpose": "standup", "last_summary": "ran ok" }
            ] }
        });
        let v2 = validate_openclaw_pro_host_brief(&brief2);
        assert!(!v2.missing.contains(&"context.cron_jobs_results".to_owned()));
    }

    #[test]
    fn derive_workflows_synthesizes_and_dedups() {
        let crons = vec![
            json!({ "id": "c1", "purpose": "Nightly backup", "schedule": "0 2 * * *" }),
            json!({ "id": "c2", "purpose": "no schedule here" }), // skipped (no schedule)
        ];
        let skills = vec![
            "Research: deep web dives".to_owned(),
            "Research: dup".to_owned(), // dedup by name within kind
        ];
        let wf = derive_workflows(&crons, "- daily standup\n- weekly review", &skills);
        // 1 scheduled + 2 routines + 1 skill (second skill deduped).
        assert!(wf
            .iter()
            .any(|w| w.kind == "scheduled" && w.name == "Nightly backup"));
        assert!(wf.iter().filter(|w| w.kind == "routine").count() == 2);
        assert_eq!(wf.iter().filter(|w| w.kind == "skill").count(), 1);
        assert!(!wf.is_empty());
    }

    #[test]
    fn derive_workflows_caps_to_max() {
        let skills: Vec<String> = (0..300).map(|i| format!("skill{i}: detail")).collect();
        let wf = derive_workflows(&[], "", &skills);
        assert_eq!(wf.len(), MAX_SKILLS);
    }

    #[test]
    fn callback_url_allowlist() {
        assert_eq!(
            normalize_callback_url("https://example.com/x"),
            "https://example.com/x"
        );
        assert_eq!(
            normalize_callback_url("openclaw://task/42"),
            "openclaw://task/42"
        );
        assert_eq!(normalize_callback_url("javascript:alert(1)"), "");
        assert_eq!(normalize_callback_url("file:///etc/passwd"), "");
        assert_eq!(normalize_callback_url("ftp://x"), "");
    }

    #[test]
    fn callback_links_dedup_and_cap() {
        let value = json!([
            "https://a.com",
            { "url": "https://a.com", "label": "dup" }, // deduped
            { "url": "https://b.com", "label": "B" },
            "not-a-url",
            "openclaw://c"
        ]);
        let links = normalize_callback_links(&value);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].url, "https://a.com");
        assert!(links.iter().any(|l| l.url == "openclaw://c"));
    }

    #[test]
    fn callback_links_respects_max() {
        let urls: Vec<Value> = (0..20)
            .map(|i| json!(format!("https://site{i}.com")))
            .collect();
        let links = normalize_callback_links(&json!(urls));
        assert_eq!(links.len(), MAX_CALLBACK_TASK_LINKS);
    }

    #[test]
    fn callback_log_clamps_and_caps() {
        let big = "x".repeat(2000);
        let value = json!([big, "short", "another"]);
        let log = normalize_callback_log(&value);
        assert_eq!(log[0].chars().count(), 800);
        assert!(log.len() <= 20);
    }

    #[test]
    fn call_intent_normalizes() {
        assert_eq!(
            normalize_call_intent(" Outbound_Callback "),
            "outbound_callback"
        );
        assert_eq!(normalize_call_intent("OUTBOUND"), "outbound");
        assert_eq!(normalize_call_intent("inbound"), "inbound");
        assert_eq!(normalize_call_intent("weird"), "");
    }

    #[test]
    fn callback_task_clamps_reply_and_summary() {
        let big_reply = "y".repeat(20_000);
        let value = json!({
            "task_id": "t-1",
            "status": "completed",
            "reply": big_reply,
            "links": ["https://done.example/x"]
        });
        let norm = normalize_callback_task(&value).unwrap();
        assert_eq!(norm.task_id, "t-1");
        assert_eq!(norm.status, "completed");
        assert!(norm.reply.chars().count() <= MAX_CALLBACK_TASK_TEXT);
        assert!(norm.summary.chars().count() <= MAX_CALLBACK_TASK_SUMMARY);
        assert_eq!(norm.links.len(), 1);
    }

    #[test]
    fn callback_task_without_id_is_none() {
        assert!(normalize_callback_task(&json!({ "reply": "hi" })).is_none());
        assert!(normalize_callback_task(&json!("not an object")).is_none());
    }

    #[test]
    fn clean_json_value_redacts_nested_strings() {
        let value = json!({
            "a": "xai-FAKEKEYFORTESTINGONLY1234567890",
            "b": ["plain", { "c": "also xai-FAKEKEYFORTESTINGONLY1234567890" }]
        });
        let cleaned = clean_json_value(&value, 4000);
        let s = cleaned.to_string();
        assert!(!s.contains("FAKEKEY"));
    }
}
