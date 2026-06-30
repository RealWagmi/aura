//! `Brief` — the host-agnostic context envelope a `HostAdapter` produces
//! and the instruction composer consumes.
//!
//! This is the V2 brief schema.
//! Every host (Claude/Codex/Hermes/OpenClaw) reads its own store into this
//! one shape; the composer then turns it into the system instructions the
//! voice model is started with. Keeping the shape here in `aura-core` — with
//! no transport or host specifics — lets every adapter and the composer
//! agree on one schema.
//!
//! ## Fail-open
//!
//! Parsing is parse-don't-validate: deserialization is **tolerant** (every
//! field has a default, unknown fields are ignored), so a thin or empty
//! brief deserializes fine and the call **always** proceeds. [`Brief::clamp`]
//! **trims** over-cap fields to the hard ceilings — it never errors and
//! never drops the call. [`Brief::validate`] is **observability only**: it
//! reports what was over cap or malformed (for logging/metrics) and never
//! gates anything. This is the deliberate inverse of a fail-closed
//! anti-pattern (a 22-field gate that returned `STOPPED` instead of a link).
//!
//! The per-field caps mirror the Hermes brief wire format exactly so the
//! contract stays honest. They are hard ceilings; readers
//! target a far smaller real budget (~30 message pairs / ~6000 tokens)
//! and the composer does priority-based dropping within that budget.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use regex::Regex;

// --- Per-field caps (mirror the Hermes brief wire format) --------------------

/// Hard ceiling on the serialized brief in bytes.
pub const PLAINTEXT_CAP: usize = 150_000;

pub const MAX_NAME: usize = 120;
pub const MAX_PRONOUNS: usize = 40;
pub const MAX_SOUL_SUMMARY: usize = 4_000;
pub const MAX_INTEREST: usize = 200;
pub const MAX_INTERESTS_COUNT: usize = 100;
pub const MAX_CURRENT_FOCUS: usize = 500;
pub const MAX_OPEN_THREAD: usize = 500;
pub const MAX_OPEN_THREADS: usize = 100;
pub const MAX_MSG_TEXT: usize = 2_000;
pub const MAX_MSG_CHANNEL: usize = 200;
pub const MAX_RECENT_MESSAGES: usize = 250;
pub const MAX_TASK_ID: usize = 200;
pub const MAX_TASK_SUMMARY: usize = 1_000;
pub const MAX_TASKS_COUNT: usize = 200;
pub const MAX_RECENT_LOG_COUNT: usize = 50;
pub const MAX_CRON_FIELD: usize = 500;
pub const MAX_CRON_JOBS_COUNT: usize = 200;
pub const MAX_LAST_RUNS_COUNT: usize = 20;
pub const MAX_PROFILE_NAME: usize = 200;
pub const MAX_PROFILE_SUMMARY: usize = 2_000;
pub const MAX_PROFILES_COUNT: usize = 100;
pub const MAX_OTHER_AGENT_NAME: usize = 200;
pub const MAX_OTHER_AGENT_SUMMARY: usize = 500;
pub const MAX_OTHER_AGENTS_COUNT: usize = 20;
pub const MAX_GREETING: usize = 2_000;
pub const MAX_NEEDLE: usize = 500;
pub const MAX_LEGACY_MD: usize = 32_000;
pub const MAX_AVAILABLE_SKILLS_COUNT: usize = 100;
pub const MAX_SETUP_LIST_COUNT: usize = 200;

/// Current brief schema version (the wire `v` field). Hermes emits `2`.
pub const BRIEF_SCHEMA_VERSION: u32 = 2;

fn default_v() -> u32 {
    BRIEF_SCHEMA_VERSION
}

// --- Truncation helpers (UTF-8 safe) -----------------------------------------

/// Truncate a string to at most `max` Unicode scalar values, on a char
/// boundary (never splits a multibyte character). The JS caps count UTF-16
/// units; char-count is a safe, close approximation for these generous caps.
fn clamp_str(s: &mut String, max: usize) {
    if s.chars().count() > max {
        *s = s.chars().take(max).collect();
    }
}

fn clamp_opt_str(s: &mut Option<String>, max: usize) {
    if let Some(v) = s {
        clamp_str(v, max);
    }
}

fn clamp_each_str(items: &mut [String], max_item: usize) {
    for item in items.iter_mut() {
        clamp_str(item, max_item);
    }
}

// --- Schema ------------------------------------------------------------------

/// The full context brief. All fields default, so `serde_json::from_str("{}")`
/// yields a valid (empty) brief — fail-open by construction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    /// Wire schema version. Defaults to [`BRIEF_SCHEMA_VERSION`].
    #[serde(default = "default_v")]
    pub v: u32,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub context: Context,
    /// Directive for how to greet (string or null). Required on the wire but
    /// fail-open here: `None` is fine.
    #[serde(default)]
    pub greeting_directive: Option<String>,

    // --- optional top-level fields the composer reads when present ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_needle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_first_call: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_task: Option<CallbackTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
}

impl Default for Brief {
    fn default() -> Self {
        Self {
            v: BRIEF_SCHEMA_VERSION,
            user: User::default(),
            context: Context::default(),
            greeting_directive: None,
            host_kind: None,
            call_intent: None,
            opening_line: None,
            onboarding_needle: None,
            is_first_call: None,
            available_skills: Vec::new(),
            callback_task: None,
            setup: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct User {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pronouns: Option<String>,
    #[serde(default)]
    pub soul_summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interests: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub current_focus: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_threads: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_messages_verbatim: Vec<RecentMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_tasks: Vec<RecentTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_jobs: Vec<CronJob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_agents: Vec<OtherAgent>,
}

/// One verbatim recent message. `role` is a free host-defined string
/// (commonly `"user"` / `"assistant"` / `"hermes"`) — not enum-gated, so
/// every host's role vocabulary survives the round trip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_iso: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentTask {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_log: Vec<String>,
    /// May be a string, object, array, or null — the consumer stringifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CronJob {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub schedule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_iso: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_runs: Vec<CronRun>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CronRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_iso: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OtherAgent {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CallbackTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferences: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_enabled: Vec<String>,
}

// --- clamp (fail-open enforcement) -------------------------------------------

impl Brief {
    /// Trim every field to its hard cap, in place. Never errors, never drops
    /// the call — over-cap content is truncated to the ceiling. This is the
    /// enforcement half of fail-open.
    pub fn clamp(&mut self) {
        self.user.clamp();
        self.context.clamp();
        clamp_opt_str(&mut self.greeting_directive, MAX_GREETING);
        clamp_opt_str(&mut self.host_kind, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.call_intent, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.opening_line, MAX_GREETING);
        clamp_opt_str(&mut self.onboarding_needle, MAX_NEEDLE);
        self.available_skills.truncate(MAX_AVAILABLE_SKILLS_COUNT);
        clamp_each_str(&mut self.available_skills, MAX_CRON_FIELD);
        if let Some(cb) = &mut self.callback_task {
            cb.clamp();
        }
        if let Some(setup) = &mut self.setup {
            setup.clamp();
        }
    }

    /// Convenience: clamp and return self (for builder-style call sites).
    pub fn clamped(mut self) -> Self {
        self.clamp();
        self
    }
}

impl User {
    fn clamp(&mut self) {
        clamp_str(&mut self.name, MAX_NAME);
        clamp_opt_str(&mut self.pronouns, MAX_PRONOUNS);
        clamp_str(&mut self.soul_summary, MAX_SOUL_SUMMARY);
        self.interests.truncate(MAX_INTERESTS_COUNT);
        clamp_each_str(&mut self.interests, MAX_INTEREST);
    }
}

impl Context {
    fn clamp(&mut self) {
        clamp_str(&mut self.current_focus, MAX_CURRENT_FOCUS);
        self.open_threads.truncate(MAX_OPEN_THREADS);
        clamp_each_str(&mut self.open_threads, MAX_OPEN_THREAD);
        self.recent_messages_verbatim.truncate(MAX_RECENT_MESSAGES);
        for m in &mut self.recent_messages_verbatim {
            m.clamp();
        }
        self.recent_tasks.truncate(MAX_TASKS_COUNT);
        for t in &mut self.recent_tasks {
            t.clamp();
        }
        self.cron_jobs.truncate(MAX_CRON_JOBS_COUNT);
        for j in &mut self.cron_jobs {
            j.clamp();
        }
        self.profiles.truncate(MAX_PROFILES_COUNT);
        for p in &mut self.profiles {
            p.clamp();
        }
        self.other_agents.truncate(MAX_OTHER_AGENTS_COUNT);
        for a in &mut self.other_agents {
            a.clamp();
        }
    }
}

impl RecentMessage {
    fn clamp(&mut self) {
        clamp_str(&mut self.text, MAX_MSG_TEXT);
        clamp_str(&mut self.channel, MAX_MSG_CHANNEL);
    }
}

impl RecentTask {
    fn clamp(&mut self) {
        clamp_str(&mut self.task_id, MAX_TASK_ID);
        clamp_str(&mut self.intent, MAX_TASK_SUMMARY);
        clamp_opt_str(&mut self.status, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.summary, MAX_TASK_SUMMARY);
        clamp_opt_str(&mut self.current_step, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.result_path, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.started_at, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.completed_at, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.failed_at, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.originated_at, MAX_CRON_FIELD);
        self.recent_log.truncate(MAX_RECENT_LOG_COUNT);
        clamp_each_str(&mut self.recent_log, MAX_TASK_SUMMARY);
    }
}

impl CronJob {
    fn clamp(&mut self) {
        clamp_str(&mut self.id, MAX_CRON_FIELD);
        clamp_str(&mut self.purpose, MAX_CRON_FIELD);
        clamp_str(&mut self.schedule, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.intent, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.tz, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.next_run_iso, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.last_status, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.last_summary, MAX_CRON_FIELD);
        self.last_runs.truncate(MAX_LAST_RUNS_COUNT);
        for r in &mut self.last_runs {
            clamp_opt_str(&mut r.ts_iso, MAX_CRON_FIELD);
            clamp_opt_str(&mut r.status, MAX_CRON_FIELD);
            clamp_opt_str(&mut r.summary, MAX_CRON_FIELD);
        }
    }
}

impl Profile {
    fn clamp(&mut self) {
        clamp_str(&mut self.name, MAX_PROFILE_NAME);
        clamp_str(&mut self.summary, MAX_PROFILE_SUMMARY);
    }
}

impl OtherAgent {
    fn clamp(&mut self) {
        clamp_str(&mut self.name, MAX_OTHER_AGENT_NAME);
        clamp_str(&mut self.summary, MAX_OTHER_AGENT_SUMMARY);
    }
}

impl CallbackTask {
    fn clamp(&mut self) {
        clamp_opt_str(&mut self.task_id, MAX_TASK_ID);
        clamp_opt_str(&mut self.intent, MAX_TASK_SUMMARY);
        clamp_opt_str(&mut self.status, MAX_CRON_FIELD);
        clamp_opt_str(&mut self.summary, MAX_TASK_SUMMARY);
    }
}

impl Setup {
    fn clamp(&mut self) {
        clamp_opt_str(&mut self.system_prompt_summary, MAX_LEGACY_MD);
        clamp_opt_str(&mut self.preferences, MAX_LEGACY_MD);
        clamp_opt_str(&mut self.default_model, MAX_CRON_FIELD);
        for list in [
            &mut self.skills,
            &mut self.plugins,
            &mut self.mcp_servers,
            &mut self.tools_enabled,
        ] {
            list.truncate(MAX_SETUP_LIST_COUNT);
            clamp_each_str(list, MAX_CRON_FIELD);
        }
    }
}

// --- validate (observability only — never gates the call) --------------------

/// One non-blocking issue found by [`Brief::validate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefIssue {
    /// Dotted path to the offending field, e.g. `context.recent_messages_verbatim[3].text`.
    pub path: String,
    /// Human-readable description.
    pub message: String,
}

/// The result of [`Brief::validate`]. `ok` is informational — a brief that is
/// not `ok` is still used (after [`Brief::clamp`]); the call never stops on it.
#[derive(Clone, Debug, PartialEq)]
pub struct BriefReport {
    pub ok: bool,
    pub issues: Vec<BriefIssue>,
    pub size_bytes: usize,
}

fn iso_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})$")
            .expect("static ISO 8601 regex is valid")
    })
}

/// Push a "length exceeds cap" issue when `len > cap`. Free function (not a
/// closure) so it doesn't hold a borrow on `issues` across the walk.
fn note_over(issues: &mut Vec<BriefIssue>, path: impl Into<String>, len: usize, cap: usize) {
    if len > cap {
        issues.push(BriefIssue {
            path: path.into(),
            message: format!("length {len} exceeds cap {cap}"),
        });
    }
}

impl Brief {
    /// Serialized size of the brief in bytes (JSON). Used for the plaintext
    /// cap check and for size-only logging (never log brief content).
    pub fn serialized_size(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(0)
    }

    /// Report over-cap and malformed fields for logging/metrics. **Never
    /// blocks** — the caller composes and dials regardless (fail-open). Run
    /// this *before* [`Brief::clamp`] to see what got trimmed.
    pub fn validate(&self) -> BriefReport {
        let mut issues = Vec::new();

        note_over(
            &mut issues,
            "user.name",
            self.user.name.chars().count(),
            MAX_NAME,
        );
        note_over(
            &mut issues,
            "user.soul_summary",
            self.user.soul_summary.chars().count(),
            MAX_SOUL_SUMMARY,
        );
        note_over(
            &mut issues,
            "user.interests",
            self.user.interests.len(),
            MAX_INTERESTS_COUNT,
        );

        note_over(
            &mut issues,
            "context.current_focus",
            self.context.current_focus.chars().count(),
            MAX_CURRENT_FOCUS,
        );
        note_over(
            &mut issues,
            "context.recent_messages_verbatim",
            self.context.recent_messages_verbatim.len(),
            MAX_RECENT_MESSAGES,
        );
        for (i, m) in self.context.recent_messages_verbatim.iter().enumerate() {
            note_over(
                &mut issues,
                format!("context.recent_messages_verbatim[{i}].text"),
                m.text.chars().count(),
                MAX_MSG_TEXT,
            );
            if let Some(ts) = &m.ts_iso {
                if !ts.is_empty() && !iso_re().is_match(ts) {
                    issues.push(BriefIssue {
                        path: format!("context.recent_messages_verbatim[{i}].ts_iso"),
                        message: "not a valid ISO 8601 timestamp".to_string(),
                    });
                }
            }
        }

        if let Some(g) = &self.greeting_directive {
            note_over(
                &mut issues,
                "greeting_directive",
                g.chars().count(),
                MAX_GREETING,
            );
        }

        let size_bytes = self.serialized_size();
        if size_bytes > PLAINTEXT_CAP {
            issues.push(BriefIssue {
                path: String::new(),
                message: format!(
                    "serialized brief is {size_bytes} bytes, exceeds plaintext cap {PLAINTEXT_CAP}"
                ),
            });
        }

        BriefReport {
            ok: issues.is_empty(),
            issues,
            size_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object_deserializes_to_default_brief() {
        // Fail-open: a `{}` brief is valid and ready to compose.
        let b: Brief = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(b, Brief::default());
        assert_eq!(b.v, BRIEF_SCHEMA_VERSION);
        assert!(b.user.name.is_empty());
        assert!(b.context.recent_messages_verbatim.is_empty());
        assert!(b.validate().ok, "empty brief reports no issues");
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        // Unknown fields ignored; missing fields default. Nothing errors.
        let json = r#"{ "user": { "name": "Stas" }, "totally_unknown": 42 }"#;
        let b: Brief = serde_json::from_str(json).expect("tolerant parse");
        assert_eq!(b.user.name, "Stas");
        assert_eq!(b.greeting_directive, None);
    }

    #[test]
    fn clamp_trims_overlong_strings_and_arrays() {
        let mut b = Brief {
            user: User {
                name: "x".repeat(MAX_NAME + 50),
                interests: vec!["i".to_string(); MAX_INTERESTS_COUNT + 10],
                ..User::default()
            },
            ..Brief::default()
        };
        b.context.recent_messages_verbatim = vec![
            RecentMessage {
                role: "user".into(),
                text: "t".repeat(MAX_MSG_TEXT + 100),
                ..RecentMessage::default()
            };
            MAX_RECENT_MESSAGES + 20
        ];

        b.clamp();

        assert_eq!(b.user.name.chars().count(), MAX_NAME);
        assert_eq!(b.user.interests.len(), MAX_INTERESTS_COUNT);
        assert_eq!(
            b.context.recent_messages_verbatim.len(),
            MAX_RECENT_MESSAGES
        );
        assert_eq!(
            b.context.recent_messages_verbatim[0].text.chars().count(),
            MAX_MSG_TEXT
        );
        // Per-field clamp does NOT guarantee the total size cap: 250 messages
        // at the 2000-char text cap is ~500 KB, well over PLAINTEXT_CAP. That
        // ceiling is the composer's budget to enforce; validate()
        // surfaces it for observability (issue with an empty path).
        let report = b.validate();
        assert!(report.size_bytes > PLAINTEXT_CAP);
        assert!(report.issues.iter().any(|i| i.path.is_empty()));
    }

    #[test]
    fn normal_brief_validates_clean_after_clamp() {
        // A realistically-sized brief clamps to fully clean.
        let mut b = Brief {
            user: User {
                name: "Stas".into(),
                soul_summary: "builds voice infra".into(),
                interests: vec!["rust".into(), "audio".into()],
                ..User::default()
            },
            ..Brief::default()
        };
        for i in 0..30 {
            b.context.recent_messages_verbatim.push(RecentMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                text: format!("message number {i}"),
                channel: "cli".into(),
                ts_iso: Some("2026-06-29T10:00:00Z".into()),
            });
        }
        b.clamp();
        let report = b.validate();
        assert!(report.ok, "unexpected issues: {:?}", report.issues);
        assert!(report.size_bytes < PLAINTEXT_CAP);
    }

    #[test]
    fn clamp_is_utf8_safe_on_multibyte() {
        // A string of multibyte chars longer than the cap must truncate on a
        // char boundary, not panic or split a code point.
        let mut b = Brief {
            user: User {
                name: "😀".repeat(MAX_NAME + 30), // 4-byte multibyte char
                ..User::default()
            },
            ..Brief::default()
        };
        b.clamp();
        assert_eq!(b.user.name.chars().count(), MAX_NAME);
        // Still valid UTF-8 (would have panicked on a byte-split).
        assert!(b.user.name.chars().all(|c| c == '😀'));
    }

    #[test]
    fn validate_reports_over_cap_without_blocking() {
        let b = Brief {
            user: User {
                name: "x".repeat(MAX_NAME + 1),
                ..User::default()
            },
            ..Brief::default()
        };
        let report = b.validate();
        assert!(!report.ok);
        assert!(report.issues.iter().any(|i| i.path == "user.name"));
        // The brief is untouched — validate never mutates or drops.
        assert_eq!(b.user.name.chars().count(), MAX_NAME + 1);
    }

    #[test]
    fn validate_flags_bad_iso_timestamp() {
        let mut b = Brief::default();
        b.context.recent_messages_verbatim.push(RecentMessage {
            role: "user".into(),
            text: "hi".into(),
            channel: "cli".into(),
            ts_iso: Some("not-a-timestamp".into()),
        });
        let report = b.validate();
        assert!(report.issues.iter().any(|i| i.path.ends_with(".ts_iso")));
    }

    #[test]
    fn accepts_well_formed_iso() {
        let mut b = Brief::default();
        b.context.recent_messages_verbatim.push(RecentMessage {
            role: "user".into(),
            text: "hi".into(),
            channel: "cli".into(),
            ts_iso: Some("2026-06-29T10:00:00Z".into()),
        });
        assert!(b.validate().ok);
    }

    #[test]
    fn round_trips_through_json() {
        let b = Brief {
            user: User {
                name: "Stas".into(),
                pronouns: Some("he/him".into()),
                soul_summary: "builds voice infra".into(),
                interests: vec!["rust".into(), "audio".into()],
            },
            greeting_directive: Some("be brief".into()),
            host_kind: Some("claude".into()),
            ..Brief::default()
        };
        let json = serde_json::to_string(&b).unwrap();
        let back: Brief = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
    }
}
