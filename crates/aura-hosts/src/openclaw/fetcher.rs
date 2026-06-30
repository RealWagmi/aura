//! OpenClaw workspace-file memory fetcher.
//!
//! Reads the local OpenClaw workspace (SYSTEM_PROMPT.md / PREFERENCES.md /
//! soul.md / messages.jsonl / skills / NOTES.md / CRON / TASKS / INTERESTS …)
//! into a [`HostMemoryCard`] with up to 9 coverage sections. This is the
//! fall-back read path (PATH B) for [`super::OpenClawAdapter::read_context`]
//! when no host-composed brief is supplied.
//!
//! For the verbatim-message target, [`parse_jsonl_messages`] parses up to a cap
//! (vs. the tail-10 [`last_jsonl_messages`]).

use aura_core::{
    redact_secrets, HostMemoryCard, HostMemoryPriority, HostMemorySection, HostMemorySource,
    HostSessionIdentity, HostToolDescriptor, ToolManifest,
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};

const DEFAULT_MAX_SECTION_BYTES: usize = 32 * 1024;
/// Upper bound on any single workspace file read in PATH B (matches PATH A's
/// guard and the JS `MAX_READ_BYTES` = 512 KB). A larger file degrades to
/// fail-open (treated as unreadable) instead of being pulled wholesale into
/// memory, where a multi-GB stray file could abort the call on allocation.
const MAX_READ_BYTES: u64 = 512 * 1024;

/// Read a workspace file, skipping (as an error) anything larger than
/// [`MAX_READ_BYTES`]. Returns the same `io::Result<String>` shape as
/// `fs::read_to_string`, so callers keep their existing match / `let Ok`.
fn read_to_string_bounded(path: &Path) -> std::io::Result<String> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_READ_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds OpenClaw fetcher read bound",
        ));
    }
    fs::read_to_string(path)
}
const DAILY_NOTE_LIMIT: usize = 8;
const TASK_FILE_LIMIT: usize = 8;
const SKILL_FILE_LIMIT: usize = 16;
const NOTE_FILE_LIMIT: usize = 12;
const MESSAGE_TAIL_LIMIT: usize = 10;

const SYSTEM_PROMPT_CANDIDATES: &[&str] = &[
    "SYSTEM_PROMPT.md",
    "system_prompt.md",
    "system-prompt.md",
    "SYSTEM.md",
    "system.md",
    "AGENT.md",
    "agent.md",
    ".openclaw/system_prompt.md",
    ".openclaw/system.md",
    ".openclaw/agent.md",
    "prompts/system.md",
];

const PREFERENCES_CANDIDATES: &[&str] = &[
    "PREFERENCES.md",
    "preferences.md",
    "CONFIG.md",
    "config.md",
    ".openclaw/preferences.md",
    ".openclaw/config.md",
    ".openclaw/config.json",
    "openclaw.config.json",
];

const ROUTINE_CANDIDATES: &[&str] = &[
    "ROUTINES.md",
    "routines.md",
    "REGULAR_WORK.md",
    "regular-work.md",
    "HABITS.md",
    "habits.md",
    "STANDING_ORDERS.md",
    "standing_orders.md",
    "standing-orders.md",
    ".openclaw/standing_orders.md",
];

const SCHEDULER_CANDIDATES: &[&str] = &[
    "CRON.md",
    "cron.md",
    "SCHEDULE.md",
    "schedule.md",
    "AUTOMATIONS.md",
    "automations.md",
    ".openclaw/cron.md",
    ".openclaw/cron.json",
    ".openclaw/automations.json",
];

const INTEREST_CANDIDATES: &[&str] = &[
    "INTERESTS.md",
    "interests.md",
    "TOPICS.md",
    "topics.md",
    "RESEARCH.md",
    "research.md",
];

const SOUL_CANDIDATES: &[&str] = &["soul.md", "SOUL.md", ".openclaw/soul.md"];

const LAST_MESSAGE_CANDIDATES: &[&str] = &[
    "messages.jsonl",
    "history.jsonl",
    ".openclaw/messages.jsonl",
    ".openclaw/history.jsonl",
    ".openclaw/session.jsonl",
];

/// The workspace-file candidates that hold the raw message log, exposed so the
/// adapter's PATH B can parse the full message history (not just the tail).
pub(crate) const MESSAGE_CANDIDATES: &[&str] = LAST_MESSAGE_CANDIDATES;

#[derive(Clone, Debug)]
pub struct OpenClawMemoryFetcher {
    workspace_dir: PathBuf,
    identity: HostSessionIdentity,
    generated_at_ms: u64,
    max_section_bytes: usize,
    session_tail: Option<String>,
    active_memory: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenClawLoadedSource {
    pub id: String,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenClawMemoryFetchReport {
    pub card: HostMemoryCard,
    pub loaded_sources: Vec<OpenClawLoadedSource>,
    pub missing_sources: Vec<String>,
    pub coverage: OpenClawMemoryCoverage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenClawMemoryCoverage {
    pub system_prompt_present: bool,
    pub preferences_present: bool,
    pub skills_present: bool,
    pub routine_activity_present: bool,
    pub scheduler_present: bool,
    pub interests_present: bool,
    pub notes_present: bool,
    pub soul_present: bool,
    pub last_messages_present: bool,
}

impl OpenClawMemoryCoverage {
    pub fn required_section_count(&self) -> usize {
        [
            self.system_prompt_present,
            self.preferences_present,
            self.skills_present,
            self.routine_activity_present,
            self.scheduler_present,
            self.interests_present,
            self.notes_present,
            self.soul_present,
            self.last_messages_present,
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    pub fn whole_user_context_present(&self) -> bool {
        self.required_section_count() == 9
    }

    fn from_card(card: &HostMemoryCard) -> Self {
        let mut coverage = Self::default();
        for section in &card.memory {
            if section.id.starts_with("openclaw.system_prompt") {
                coverage.system_prompt_present = true;
            } else if section.id.starts_with("openclaw.preferences")
                || section.id.starts_with("openclaw.configuration")
            {
                coverage.preferences_present = true;
            } else if section.id.starts_with("openclaw.skill") {
                coverage.skills_present = true;
            } else if section.id.starts_with("openclaw.routine") {
                coverage.routine_activity_present = true;
            } else if section.id.starts_with("openclaw.scheduler") {
                coverage.scheduler_present = true;
            } else if section.id.starts_with("openclaw.interests") {
                coverage.interests_present = true;
            } else if section.id.starts_with("openclaw.notes") {
                coverage.notes_present = true;
            } else if section.id.starts_with("openclaw.soul") {
                coverage.soul_present = true;
            } else if section.id.starts_with("openclaw.last_messages") {
                coverage.last_messages_present = true;
            }
        }
        coverage
    }
}

struct MemoryFetchBuffers<'a> {
    card: &'a mut HostMemoryCard,
    loaded_sources: &'a mut Vec<OpenClawLoadedSource>,
    missing_sources: &'a mut Vec<String>,
}

#[derive(Clone, Debug)]
struct SectionSpec<'a> {
    id: &'a str,
    label: &'a str,
    source: HostMemorySource,
    priority: HostMemoryPriority,
}

impl<'a> SectionSpec<'a> {
    fn new(
        id: &'a str,
        label: &'a str,
        source: HostMemorySource,
        priority: HostMemoryPriority,
    ) -> Self {
        Self {
            id,
            label,
            source,
            priority,
        }
    }
}

impl OpenClawMemoryFetcher {
    pub fn new(workspace_dir: impl Into<PathBuf>, identity: HostSessionIdentity) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            identity,
            generated_at_ms: 0,
            max_section_bytes: DEFAULT_MAX_SECTION_BYTES,
            session_tail: None,
            active_memory: None,
        }
    }

    pub fn with_generated_at_ms(mut self, generated_at_ms: u64) -> Self {
        self.generated_at_ms = generated_at_ms;
        self
    }

    pub fn with_max_section_bytes(mut self, max_section_bytes: usize) -> Self {
        self.max_section_bytes = max_section_bytes.max(256);
        self
    }

    pub fn with_session_tail(mut self, session_tail: impl Into<String>) -> Self {
        self.session_tail = Some(session_tail.into());
        self
    }

    pub fn with_active_memory(mut self, active_memory: impl Into<String>) -> Self {
        self.active_memory = Some(active_memory.into());
        self
    }

    /// The workspace directory this fetcher reads from.
    pub fn workspace_dir(&self) -> &std::path::Path {
        &self.workspace_dir
    }

    pub fn fetch(&self) -> OpenClawMemoryFetchReport {
        let mut card = HostMemoryCard::new(self.identity.clone(), self.generated_at_ms);
        card.tools = default_openclaw_tool_manifest();
        let mut loaded_sources = Vec::new();
        let mut missing_sources = Vec::new();

        {
            let mut buffers = MemoryFetchBuffers {
                card: &mut card,
                loaded_sources: &mut loaded_sources,
                missing_sources: &mut missing_sources,
            };

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.system_prompt",
                    "OpenClaw agent system prompt",
                    HostMemorySource::AgentSystemPrompt,
                    HostMemoryPriority::Critical,
                ),
                SYSTEM_PROMPT_CANDIDATES,
            );

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.preferences",
                    "OpenClaw user configuration and preferences",
                    HostMemorySource::Preferences,
                    HostMemoryPriority::Critical,
                ),
                PREFERENCES_CANDIDATES,
            );

            self.push_skill_sections(&mut buffers);

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.routine_activity",
                    "OpenClaw regular user activity",
                    HostMemorySource::RoutineActivity,
                    HostMemoryPriority::High,
                ),
                ROUTINE_CANDIDATES,
            );

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.scheduler",
                    "OpenClaw cron jobs and scheduled work",
                    HostMemorySource::Scheduler,
                    HostMemoryPriority::High,
                ),
                SCHEDULER_CANDIDATES,
            );

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.interests",
                    "OpenClaw user interests",
                    HostMemorySource::Interest,
                    HostMemoryPriority::Medium,
                ),
                INTEREST_CANDIDATES,
            );

            self.push_first_existing_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.soul",
                    "OpenClaw soul.md",
                    HostMemorySource::Soul,
                    HostMemoryPriority::Critical,
                ),
                SOUL_CANDIDATES,
            );

            self.push_optional_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.notes",
                    "OpenClaw notes",
                    HostMemorySource::Notes,
                    HostMemoryPriority::Medium,
                ),
                self.workspace_dir.join("NOTES.md"),
            );

            self.push_markdown_dir(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.notes_file",
                    "OpenClaw note",
                    HostMemorySource::Notes,
                    HostMemoryPriority::Medium,
                ),
                self.workspace_dir.join("notes"),
                NOTE_FILE_LIMIT,
            );

            self.push_last_messages_section(&mut buffers);

            self.push_optional_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.memory",
                    "OpenClaw MEMORY.md",
                    HostMemorySource::LongTermMemory,
                    HostMemoryPriority::High,
                ),
                self.workspace_dir.join("MEMORY.md"),
            );

            self.push_markdown_dir(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.daily_note",
                    "OpenClaw daily note",
                    HostMemorySource::DailyNote,
                    HostMemoryPriority::Medium,
                ),
                self.workspace_dir.join("memory"),
                DAILY_NOTE_LIMIT,
            );

            self.push_optional_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.dreams",
                    "OpenClaw DREAMS.md",
                    HostMemorySource::Other,
                    HostMemoryPriority::Low,
                ),
                self.workspace_dir.join("DREAMS.md"),
            );

            self.push_optional_file(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.tasks",
                    "OpenClaw TASKS.md",
                    HostMemorySource::TaskState,
                    HostMemoryPriority::Medium,
                ),
                self.workspace_dir.join("TASKS.md"),
            );

            self.push_markdown_dir(
                &mut buffers,
                SectionSpec::new(
                    "openclaw.task_file",
                    "OpenClaw task file",
                    HostMemorySource::TaskState,
                    HostMemoryPriority::Medium,
                ),
                self.workspace_dir.join("tasks"),
                TASK_FILE_LIMIT,
            );

            if let Some(session_tail) = self.session_tail.as_deref() {
                self.push_inline_section(
                    &mut buffers,
                    SectionSpec::new(
                        "openclaw.session_tail",
                        "OpenClaw session tail",
                        HostMemorySource::SessionTail,
                        HostMemoryPriority::High,
                    ),
                    session_tail,
                );
            }

            if let Some(active_memory) = self.active_memory.as_deref() {
                self.push_inline_section(
                    &mut buffers,
                    SectionSpec::new(
                        "openclaw.active_memory",
                        "OpenClaw active memory",
                        HostMemorySource::ActiveMemory,
                        HostMemoryPriority::High,
                    ),
                    active_memory,
                );
            }
        }

        let coverage = OpenClawMemoryCoverage::from_card(&card);
        card.metadata.insert(
            "host_memory_fetcher".to_owned(),
            json!({
                "host": "open_claw",
                "loaded_sources": loaded_sources.iter().map(|source| source.id.as_str()).collect::<Vec<_>>(),
                "missing_sources": missing_sources.clone(),
                "whole_user_context": coverage.whole_user_context_present(),
                "required_section_count": coverage.required_section_count(),
                "coverage": {
                    "system_prompt_present": coverage.system_prompt_present,
                    "preferences_present": coverage.preferences_present,
                    "skills_present": coverage.skills_present,
                    "routine_activity_present": coverage.routine_activity_present,
                    "scheduler_present": coverage.scheduler_present,
                    "interests_present": coverage.interests_present,
                    "notes_present": coverage.notes_present,
                    "soul_present": coverage.soul_present,
                    "last_messages_present": coverage.last_messages_present,
                },
            }),
        );

        OpenClawMemoryFetchReport {
            card,
            loaded_sources,
            missing_sources,
            coverage,
        }
    }

    fn push_first_existing_file(
        &self,
        buffers: &mut MemoryFetchBuffers<'_>,
        spec: SectionSpec<'_>,
        candidates: &[&str],
    ) {
        for candidate in candidates {
            let path = self.workspace_dir.join(candidate);
            match read_to_string_bounded(&path) {
                Ok(text) if !text.trim().is_empty() => {
                    buffers.card.memory.push(self.section(
                        spec.id,
                        spec.label,
                        spec.source,
                        spec.priority,
                        &text,
                    ));
                    buffers.loaded_sources.push(OpenClawLoadedSource {
                        id: spec.id.to_owned(),
                        path: Some(path),
                    });
                    return;
                }
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    buffers
                        .missing_sources
                        .push(format!("{}:io:{err}", spec.id));
                    return;
                }
            }
        }
        buffers.missing_sources.push(format!("{}:missing", spec.id));
    }

    fn push_skill_sections(&self, buffers: &mut MemoryFetchBuffers<'_>) {
        let mut skill_files = Vec::new();
        for root in ["skills", ".openclaw/skills"] {
            let dir = self.workspace_dir.join(root);
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    let skill_path = path.join("SKILL.md");
                    if skill_path.is_file() {
                        skill_files.push(skill_path);
                    }
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                    skill_files.push(path);
                }
            }
        }

        skill_files.sort();
        skill_files.truncate(SKILL_FILE_LIMIT);
        if skill_files.is_empty() {
            buffers
                .missing_sources
                .push("openclaw.skill:missing".to_owned());
            return;
        }

        for path in skill_files {
            let name = path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .or_else(|| path.file_stem().and_then(|name| name.to_str()))
                .unwrap_or("skill")
                .to_owned();
            let id = format!("openclaw.skill.{}", normalize_id_fragment(&name));
            let label = format!("OpenClaw skill: {name}");
            self.push_optional_file(
                buffers,
                SectionSpec::new(
                    &id,
                    &label,
                    HostMemorySource::Skill,
                    HostMemoryPriority::High,
                ),
                path,
            );
        }
    }

    fn push_last_messages_section(&self, buffers: &mut MemoryFetchBuffers<'_>) {
        for candidate in LAST_MESSAGE_CANDIDATES {
            let path = self.workspace_dir.join(candidate);
            let Ok(text) = read_to_string_bounded(&path) else {
                continue;
            };
            let messages = last_jsonl_messages(&text, MESSAGE_TAIL_LIMIT);
            if messages.is_empty() {
                continue;
            }
            self.push_inline_section(
                buffers,
                SectionSpec::new(
                    "openclaw.last_messages",
                    "OpenClaw last ten messages",
                    HostMemorySource::MessageTail,
                    HostMemoryPriority::High,
                ),
                &messages.join("\n"),
            );
            // Retroactively attach the source path to the inline section just
            // pushed (`push_inline_section` records inline sections with
            // `path: None`). This is position-dependent on the inline push
            // having appended exactly one entry as its last action (guaranteed
            // by the non-empty `messages` guard above).
            if let Some(last) = buffers.loaded_sources.last_mut() {
                last.path = Some(path);
            }
            return;
        }
        buffers
            .missing_sources
            .push("openclaw.last_messages:missing".to_owned());
    }

    fn push_optional_file(
        &self,
        buffers: &mut MemoryFetchBuffers<'_>,
        spec: SectionSpec<'_>,
        path: PathBuf,
    ) {
        match read_to_string_bounded(&path) {
            Ok(text) if !text.trim().is_empty() => {
                buffers.card.memory.push(self.section(
                    spec.id,
                    spec.label,
                    spec.source,
                    spec.priority,
                    &text,
                ));
                buffers.loaded_sources.push(OpenClawLoadedSource {
                    id: spec.id.to_owned(),
                    path: Some(path),
                });
            }
            Ok(_) => buffers.missing_sources.push(format!("{}:empty", spec.id)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                buffers.missing_sources.push(format!("{}:missing", spec.id));
            }
            Err(err) => buffers
                .missing_sources
                .push(format!("{}:io:{err}", spec.id)),
        }
    }

    fn push_markdown_dir(
        &self,
        buffers: &mut MemoryFetchBuffers<'_>,
        spec: SectionSpec<'_>,
        dir: PathBuf,
        limit: usize,
    ) {
        let Ok(entries) = fs::read_dir(&dir) else {
            buffers.missing_sources.push(format!("{}:missing", spec.id));
            return;
        };

        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .collect();
        files.sort();
        files.reverse();

        if files.is_empty() {
            buffers.missing_sources.push(format!("{}:empty", spec.id));
            return;
        }

        for path in files.into_iter().take(limit) {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("unknown")
                .to_owned();
            let id = format!("{}.{}", spec.id, stem);
            let label = format!("{}: {}", spec.label, stem);
            self.push_optional_file(
                buffers,
                SectionSpec::new(&id, &label, spec.source.clone(), spec.priority.clone()),
                path,
            );
        }
    }

    fn push_inline_section(
        &self,
        buffers: &mut MemoryFetchBuffers<'_>,
        spec: SectionSpec<'_>,
        text: &str,
    ) {
        if text.trim().is_empty() {
            return;
        }
        buffers.card.memory.push(self.section(
            spec.id,
            spec.label,
            spec.source,
            spec.priority,
            text,
        ));
        buffers.loaded_sources.push(OpenClawLoadedSource {
            id: spec.id.to_owned(),
            path: None,
        });
    }

    fn section(
        &self,
        id: &str,
        label: &str,
        source: HostMemorySource,
        priority: HostMemoryPriority,
        text: &str,
    ) -> HostMemorySection {
        HostMemorySection::untrusted(
            id,
            label,
            source,
            priority,
            redact_secrets(&truncate_to_boundary(text, self.max_section_bytes)),
        )
    }
}

pub(crate) fn default_openclaw_tool_manifest() -> ToolManifest {
    ToolManifest::new(vec![
        HostToolDescriptor::read_only("memory_search", "Search OpenClaw memory"),
        HostToolDescriptor::read_only("memory_get", "Read a specific OpenClaw memory item"),
        HostToolDescriptor {
            name: "openclaw_agent_consult".to_owned(),
            description: "Ask the bound OpenClaw agent/session to handle a bounded task".to_owned(),
            read_only: false,
            destructive: false,
            requires_confirmation: false,
        },
        HostToolDescriptor::confirmed_action(
            "openclaw_confirmed_action",
            "Run a destructive OpenClaw action only after typed or clicked confirmation",
        ),
    ])
}

fn normalize_id_fragment(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    normalized.trim_matches('_').to_owned()
}

fn last_jsonl_messages(text: &str, limit: usize) -> Vec<String> {
    let mut messages = Vec::new();
    for line in text.lines().rev() {
        if messages.len() >= limit {
            break;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some((role, body)) = message_from_json_value(&value) else {
            continue;
        };
        let clean = body.trim();
        if clean.is_empty() {
            continue;
        }
        messages.push(format!("[{}] {}", role.trim(), clean));
    }
    messages.reverse();
    messages
}

/// One parsed verbatim message from the raw OpenClaw message log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedMessage {
    pub role: String,
    pub text: String,
    pub channel: String,
    pub ts_iso: Option<String>,
}

/// Parse up to the latest `cap` messages from a raw JSONL message log,
/// preserving chronological order. Mirrors the JS `parseMessages` role mapping
/// (`user`/`human`/`client`/`owner` → `user`, everything else → `openclaw`).
/// Unlike [`last_jsonl_messages`] (which formats a `[role] text` tail of 10),
/// this returns structured turns for the brief's verbatim-message target.
pub(crate) fn parse_jsonl_messages(text: &str, cap: usize) -> Vec<ParsedMessage> {
    let mut messages = Vec::new();
    for line in text.lines() {
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let role_raw = object
            .get("role")
            .or_else(|| object.get("author"))
            .or_else(|| object.get("speaker"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let role = if matches!(role_raw.as_str(), "user" | "human" | "client" | "owner") {
            "user"
        } else {
            "openclaw"
        }
        .to_owned();
        let Some(body) = object
            .get("content")
            .and_then(message_content_text)
            .or_else(|| object.get("text").and_then(message_content_text))
            .or_else(|| object.get("message").and_then(message_content_text))
            .or_else(|| object.get("body").and_then(message_content_text))
        else {
            continue;
        };
        let text = body.trim();
        if text.is_empty() {
            continue;
        }
        let ts_iso = object
            .get("ts_iso")
            .or_else(|| object.get("timestamp"))
            .or_else(|| object.get("created_at"))
            .or_else(|| object.get("time"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned);
        let channel = object
            .get("channel")
            .or_else(|| object.get("platform"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        messages.push(ParsedMessage {
            role,
            text: text.to_owned(),
            channel,
            ts_iso,
        });
    }
    let drop = messages.len().saturating_sub(cap);
    if drop > 0 {
        messages.drain(0..drop);
    }
    messages
}

fn message_from_json_value(value: &serde_json::Value) -> Option<(String, String)> {
    let object = value.as_object()?;
    let role = object
        .get("role")
        .or_else(|| object.get("speaker"))
        .or_else(|| object.get("author"))
        .or_else(|| object.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or("message")
        .to_owned();
    let body = object
        .get("content")
        .and_then(message_content_text)
        .or_else(|| object.get("text").and_then(message_content_text))
        .or_else(|| object.get("message").and_then(message_content_text))?;
    Some((role, body))
}

fn message_content_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(items) = value.as_array() {
        let mut parts = Vec::new();
        for item in items {
            if let Some(text) = item
                .get("text")
                .and_then(|value| value.as_str())
                .or_else(|| item.get("content").and_then(|value| value.as_str()))
            {
                parts.push(text.to_owned());
            }
        }
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    None
}

fn truncate_to_boundary(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let suffix = "...[truncated]";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &input[..end], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn identity() -> HostSessionIdentity {
        HostSessionIdentity::openclaw("principal", "agent-1", "sess-1")
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn fetch_collects_coverage_sections_from_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, "SYSTEM_PROMPT.md", "You are OpenClaw.");
        write(dir, "PREFERENCES.md", "Be concise and warm.");
        write(dir, "soul.md", "I care about good craft.");
        write(dir, "INTERESTS.md", "rust\naudio\nlatency");
        write(dir, "ROUTINES.md", "daily standup at 9");
        write(dir, "CRON.md", "0 9 * * * standup");
        write(dir, "NOTES.md", "remember the launch date");
        write(
            dir,
            "skills/research/SKILL.md",
            "# Research\nDeep web dives.",
        );
        write(
            dir,
            "messages.jsonl",
            "{\"role\":\"user\",\"content\":\"hi\"}\n{\"role\":\"assistant\",\"content\":\"hello\"}\n",
        );

        let report = OpenClawMemoryFetcher::new(dir, identity()).fetch();
        let cov = &report.coverage;
        assert!(cov.system_prompt_present);
        assert!(cov.preferences_present);
        assert!(cov.soul_present);
        assert!(cov.interests_present);
        assert!(cov.routine_activity_present);
        assert!(cov.scheduler_present);
        assert!(cov.notes_present);
        assert!(cov.skills_present);
        assert!(cov.last_messages_present);
        assert!(cov.whole_user_context_present());
        // All card sections are untrusted + redacted.
        assert!(report.card.memory.iter().all(|s| s.untrusted && s.redacted));
    }

    #[test]
    fn fetch_on_empty_dir_yields_no_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let report = OpenClawMemoryFetcher::new(tmp.path(), identity()).fetch();
        assert_eq!(report.coverage.required_section_count(), 0);
        assert!(!report.coverage.whole_user_context_present());
        assert!(!report.missing_sources.is_empty());
    }

    #[test]
    fn parse_jsonl_messages_maps_roles_and_caps() {
        let mut lines = String::new();
        for i in 0..300 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            lines.push_str(&format!(
                "{{\"role\":\"{role}\",\"content\":\"msg {i}\"}}\n"
            ));
        }
        let parsed = parse_jsonl_messages(&lines, 250);
        assert_eq!(parsed.len(), 250);
        // The oldest 50 were dropped; first kept is msg 50 (even -> user).
        assert_eq!(parsed[0].text, "msg 50");
        assert_eq!(parsed[0].role, "user");
        assert!(parsed.iter().any(|m| m.role == "openclaw"));
    }

    #[test]
    fn parse_jsonl_messages_skips_blank_and_invalid() {
        let raw = "\n  \nnot json\n{\"role\":\"client\",\"content\":\"hey\"}\n{\"role\":\"bot\"}\n";
        let parsed = parse_jsonl_messages(raw, 250);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].role, "user"); // client -> user
        assert_eq!(parsed[0].text, "hey");
    }
}
