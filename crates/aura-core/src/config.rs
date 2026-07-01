//! Aura configuration — the typed schema, its defaults, and safe
//! load/save.
//!
//! Why this exists
//! ===============
//! This is the single source of truth for every tunable: provider /
//! model selection, voice latency and audio gains, compaction
//! thresholds, safety posture, and the per-worker (Codex / Claude) hot
//! intervals. Defaults live in the `Default` impls here and are pinned
//! by tests so a casual edit can't silently move product behaviour.
//!
//! Loading is defensive: the config file is read kernel-bounded (see
//! `MAX_CONFIG_FILE_BYTES`) because an oversized config is corruption
//! or an attempt to smuggle a payload behind innocuous fields; `save`
//! goes through the private-FS truncated writer so the file stays
//! `0o600`. The hot-interval override helpers clamp to
//! `HOT_INTERVAL_FLOOR_MS..=HOT_INTERVAL_CEIL_MS` so neither a runaway
//! caller nor a hand-edited file can drive the feeder loop too hot or
//! too cold.

use crate::private_fs::write_private_truncated;
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    str::FromStr,
};

/// Maximum size of an Aura config JSON we'll read in one shot. The
/// config is small structured JSON; an oversized file is corruption or
/// an attempt to hide a payload behind innocuous-looking fields. Same
/// kernel-bounded read pattern as `aura-discord::read_reason`.
const MAX_CONFIG_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackMode {
    #[default]
    PingFirst,
    SpeakImmediately,
    SilentNotification,
}

impl fmt::Display for CallbackMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::PingFirst => "ping_first",
            Self::SpeakImmediately => "speak_immediately",
            Self::SilentNotification => "silent_notification",
        };
        f.write_str(value)
    }
}

impl FromStr for CallbackMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // `hangup` / `auto_hangup` map to PingFirst because that's the right
        // semantic for the dispatch-then-end-voice-session pattern: when the
        // call is over and a result lands later, the user gets a ping first
        // (the call can't speak immediately — there's no live call). The
        // model invents `hangup` as a callback mode when reasoning about
        // async-handoff dispatch (observed in production); aliasing
        // here makes the inference correct rather than forcing a retry.
        match value.trim().to_ascii_lowercase().as_str() {
            "ping_first" | "ping" | "ask_first" | "hangup" | "auto_hangup" => Ok(Self::PingFirst),
            "speak_immediately" | "speak" | "immediate" => Ok(Self::SpeakImmediately),
            "silent_notification" | "silent" | "notify" => Ok(Self::SilentNotification),
            other => Err(format!(
                "unknown callback mode '{other}'. Use ping_first, speak_immediately, or silent_notification (aliases: ping, speak, silent, hangup, auto_hangup)."
            )),
        }
    }
}

/// How Aura reaches a given agent's runtime — the wire-transport class
/// shared by `live-state.json`, `active-task.json`, and the connection
/// profiles in [`AuraConfig::connections`]. Defined here in `aura-core`
/// so both `aura-cli` (the writer) and this config module reuse one
/// canonical enum rather than re-deriving stringly-typed variants.
///
/// On-disk / wire form is snake_case (`"local"`, `"direct"`, `"relay"`)
/// to match the JSON keys in the multi-agent control-center contracts.
///
/// - `Local`: the agent runs as a local subprocess on this machine.
/// - `Direct`: a direct connection to a remote runtime endpoint (HTTPS).
/// - `Relay`: routed through the runtime-inbox relay (`wss://…`), never
///   opening a second principal WS against the live runtime.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransport {
    Local,
    Direct,
    Relay,
}

/// Last-known reachability of an agent's runtime, surfaced on the
/// control panel. Written by the runtime against an elapsed-since-last-
/// contact threshold (`connected` ≤30s, `stale` 30–90s, `unreachable`
/// >90s or an explicit failure such as `runtime_offline`).
///
/// Defined in `aura-core` alongside [`AgentTransport`] so the writer
/// (`aura-cli`) and config consumers share one enum. On-disk / wire form
/// is snake_case (`"connected"`, `"stale"`, `"unreachable"`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHealth {
    Connected,
    Stale,
    Unreachable,
}

/// Which realtime voice provider drives Aura's speech in/out.
/// Wired into the Settings UI so the user can pick Grok or OpenAI
/// without editing JSON. The runtime routes URL + auth + model
/// selection off this enum.
///
/// On-disk format: the canonical values are `"grok_realtime"` and
/// `"openai_realtime"` (with the `_realtime` suffix). The aura-orb
/// crate already writes those strings via its NSPopUpButton picker,
/// and the names are explicit about *which* API surface we mean.
/// The shorter aliases `"grok"` and `"openai"` are accepted on read
/// for ergonomics (CLI users typing `provider.engine = "openai"`)
/// but every Serialize round-trip emits the long form so the file
/// stays in lockstep with the orb picker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoiceEngine {
    #[default]
    #[serde(rename = "grok_realtime", alias = "grok")]
    Grok,
    #[serde(rename = "openai_realtime", alias = "openai")]
    OpenAI,
}

impl VoiceEngine {
    /// Canonical on-disk string. Matches what `aura-orb` writes via
    /// its settings panel (`OPENAI_REALTIME_ENGINE` /
    /// `GROK_REALTIME_ENGINE` constants in
    /// `crates/aura-orb/src/window.rs`).
    pub fn as_str(self) -> &'static str {
        match self {
            VoiceEngine::Grok => "grok_realtime",
            VoiceEngine::OpenAI => "openai_realtime",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Which realtime backend drives the voice loop. Defaults to
    /// Grok; setting to OpenAI switches the WebSocket endpoint, the
    /// auth header (OPENAI_API_KEY), and the default model.
    #[serde(default)]
    pub engine: VoiceEngine,
    pub model: String,
    /// Legacy single-voice field. Held for backward compatibility
    /// with older configs and as a "currently active" mirror. New
    /// code should call `effective_voice()` so we pick from the
    /// engine-specific field below.
    pub voice: String,
    /// Per-engine voice memory. The user can pick "alloy" for OpenAI
    /// and "eve" for Grok; flipping the engine dropdown then doesn't
    /// clobber the choice they made for the other provider. Mirrors
    /// what `aura-orb` writes via its settings panel.
    #[serde(default)]
    pub grok_voice: Option<String>,
    #[serde(default)]
    pub openai_voice: Option<String>,
    #[serde(default = "default_latency_target_ms")]
    pub latency_target_ms: u64,
    /// Optional sampling temperature passed to Grok in `session.update`.
    /// Autoresearched: temperature 0.5 produces the highest median score
    /// (+142 / 18-perfect 3 of 3) and the lowest std (3.1) on the v89
    /// 4-bucket prompt. Default `Some(0.5)` keeps live behavior aligned
    /// with the bench winner; set to `None` to use Grok's server default.
    #[serde(default = "default_temperature")]
    pub temperature: Option<f64>,
}

impl ProviderConfig {
    /// Pick the voice to actually send in `session.update`, based on
    /// the configured engine. Falls back to the legacy `voice` field
    /// (and finally a per-engine hard-coded default) so the runtime
    /// never crashes on a missing field.
    pub fn effective_voice(&self) -> &str {
        let fallback_voice = |default_voice| {
            if !self.voice.is_empty() {
                self.voice.as_str()
            } else {
                default_voice
            }
        };
        match self.engine {
            VoiceEngine::Grok => self
                .grok_voice
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback_voice("eve")),
            VoiceEngine::OpenAI => self
                .openai_voice
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let legacy = self.voice.as_str();
                    is_openai_realtime_voice(legacy).then_some(legacy)
                })
                .unwrap_or("alloy"),
        }
    }
}

fn is_openai_realtime_voice(voice: &str) -> bool {
    matches!(
        voice,
        "alloy" | "ash" | "ballad" | "coral" | "echo" | "sage" | "shimmer" | "verse"
    )
}

fn default_temperature() -> Option<f64> {
    Some(0.5)
}

fn default_latency_target_ms() -> u64 {
    800
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            engine: VoiceEngine::default(),
            model: "grok-voice-think-fast-1.0".to_owned(),
            voice: "eve".to_owned(),
            grok_voice: None,
            openai_voice: None,
            latency_target_ms: 800,
            temperature: default_temperature(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyConfig {
    pub local_only: bool,
    pub require_voice_approval: bool,
    pub require_cancel_confirmation: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            local_only: true,
            require_voice_approval: true,
            require_cancel_confirmation: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryConfig {
    pub path: PathBuf,
    pub max_events: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from(".aura/history.jsonl"),
            max_events: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub transcript_path: Option<PathBuf>,
    /// Directory containing Claude Code transcript JSONL files. When
    /// `transcript_path` is unset, Aura picks the most-recently-modified
    /// `.jsonl` here as the active session.
    pub transcripts_dir: Option<PathBuf>,
    pub hooks_dir: Option<PathBuf>,
    pub execute_tasks: bool,
    pub cli_path: Option<PathBuf>,
    pub permission_mode: String,
    pub allowed_tools: Vec<String>,
    pub max_budget_usd: Option<String>,
    /// Optional pin for the in-call dispatch model (`claude -p --model <m>`).
    /// `None` (default) → the model is resolved per-dispatch from the live chat
    /// transcript (Scheme 1); set this to force a specific model regardless of
    /// what the chat session is using. An explicit value always wins over the
    /// transcript auto-detection.
    pub dispatch_model: Option<String>,
    // ---------------- Feeder parity (Codex feeder mirror) ----------------
    // Claude path's `start_context_feeder` previously hardcoded model
    // names + cycle interval + research max-in-flight. The Codex path
    // had `CodexConfig::{hot_interval_ms,feeder_mode,hot_model,...}`
    // for a long time. These fields close the parity gap so
    // `--agent claude` honours the same operator knobs as
    // `--agent codex`. Fields default to the previously-hardcoded
    // values so existing on-disk configs keep their current behaviour.
    /// Cycle tick for Claude's ambient feeder, in milliseconds.
    /// Mirrors `CodexConfig::hot_interval_ms`. Pre-parity behaviour
    /// was 3000ms hardcoded in `aura_context_feeder::CycleConfig`.
    pub hot_interval_ms: u64,
    /// Proactivity preset for the Claude feeder. Mirrors
    /// `CodexConfig::feeder_mode`. Same alphabet
    /// (`"aggressive"` / `"balanced"` / `"conservative"` / etc.); the
    /// downstream subagent reads the string verbatim.
    pub feeder_mode: String,
    /// Fast feeder model. Pre-parity: `"claude-sonnet-4-6"` hardcoded
    /// in `aura-cli::feeder_setup::start_context_feeder`. Mirrors
    /// `CodexConfig::hot_model`.
    pub hot_model: String,
    /// Research feeder model. Pre-parity: `"claude-sonnet-4-6"`
    /// hardcoded a second time. Operators may want a faster /
    /// cheaper model here than the fast tier; the parity field lets
    /// them choose. Mirrors `CodexConfig::research_model`.
    pub research_model: String,
    /// Cap on concurrent research subagent runs. Pre-parity: 3
    /// hardcoded. Mirrors `CodexConfig::research_max_in_flight`
    /// (which defaults to 2 — Claude defaults to 3 for backward
    /// compat, intentionally diverging here so existing Claude
    /// users see no behaviour change).
    pub research_max_in_flight: usize,
    /// Search fanout knob. Mirrors `CodexConfig::search_fanout` for
    /// schema parity (so a config that switches `worker` from
    /// `"codex"` to `"claude"` round-trips cleanly), but is **NOT
    /// consumed by the Claude feeder runtime today**. Codex's
    /// `local_search` runs `fanout` parallel ripgrep queries against
    /// the working tree on each digest tick and feeds the hits into
    /// Spark; the Claude feeder takes a different shape — Sonnet
    /// gets `Read`, `Grep`, and `Bash` from the standard tools list,
    /// runs its own search inline during a digest turn, and we let
    /// the model decide how aggressively to dig. Wiring `search_fanout`
    /// would mean either prepending a parallel-search step before
    /// Sonnet's turn (duplicates Sonnet's own search) or constraining
    /// Sonnet's tool budget (turns the feeder into a different
    /// architecture). Neither change has user demand today.
    ///
    /// Operators tuning the Claude feeder's search behaviour should
    /// adjust the role prompt or the prefill, not this field.
    /// Pinned by an integration test in `aura-cli::feeder_setup`
    /// that asserts `build_cycle_config`, `build_research_config`,
    /// and `build_system_prompt` all leave `search_fanout` unread.
    pub search_fanout: usize,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            transcript_path: None,
            transcripts_dir: None,
            hooks_dir: None,
            execute_tasks: false,
            cli_path: None,
            permission_mode: "acceptEdits".to_owned(),
            allowed_tools: vec![
                "Edit".to_owned(),
                "MultiEdit".to_owned(),
                "Write".to_owned(),
                "Bash(cargo *)".to_owned(),
                "Bash(rg *)".to_owned(),
                "Bash(git diff *)".to_owned(),
            ],
            max_budget_usd: Some("1.00".to_owned()),
            dispatch_model: None,
            // Parity defaults — must match the pre-parity hardcoded
            // constants in `aura-cli::feeder_setup` so a config that
            // omits these keys keeps its old behaviour.
            hot_interval_ms: 3000,
            feeder_mode: "balanced".to_owned(),
            hot_model: "claude-sonnet-4-6".to_owned(),
            research_model: "claude-sonnet-4-6".to_owned(),
            research_max_in_flight: 3,
            search_fanout: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// Codex binary to spawn for `codex app-server --listen stdio://`.
    /// Defaults to PATH lookup for `codex`.
    pub app_server_bin: PathBuf,
    /// Optional worker model override. `None` means inherit the user's
    /// Codex app-server/thread default.
    pub worker_model: Option<String>,
    /// Permission posture for dispatched work. `inherit` deliberately
    /// sends no sandbox/approval override, so the active Codex thread's
    /// configured permissions remain authoritative.
    pub worker_authority: String,
    /// Persisted Codex worker thread binding for this project.
    pub session_path: PathBuf,
    /// On macOS, open the Codexini coordinator thread after dispatch so
    /// Codex Desktop refreshes externally-started task turns.
    pub open_desktop_thread_on_task: bool,
    /// Ultra-fast code-aware feeder model exposed by Codex app-server.
    pub hot_model: String,
    /// Stronger research feeder model exposed by Codex app-server.
    pub research_model: String,
    /// Proactivity preset for the Codex feeder.
    pub feeder_mode: String,
    pub hot_interval_ms: u64,
    pub search_fanout: usize,
    pub research_max_in_flight: usize,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            app_server_bin: PathBuf::from("codex"),
            worker_model: None,
            worker_authority: "inherit".to_owned(),
            session_path: PathBuf::from(".aura/codex/session.json"),
            open_desktop_thread_on_task: true,
            hot_model: "gpt-5.3-codex-spark".to_owned(),
            research_model: "gpt-5.4-mini".to_owned(),
            feeder_mode: "aggressive".to_owned(),
            hot_interval_ms: 1500,
            search_fanout: 8,
            research_max_in_flight: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointConfig {
    /// Maximum number of checkpoint events held in memory at once.
    pub max_in_memory: usize,
    /// Optional JSONL append path. `None` means in-memory only — the
    /// default. The on-disk file is append-only with no rotation, so
    /// a long-running session would grow it without bound; off by
    /// default is privacy-preserving and avoids the disk-growth
    /// concern. Set a path to also persist for cross-session
    /// debugging.
    pub log_path: Option<PathBuf>,
    /// How many recent checkpoints `get_context_summary` weaves into the
    /// speech briefing. Capped against `max_in_memory`.
    pub recent_for_summary: usize,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            max_in_memory: 64,
            // INVARIANT: default is in-memory only. Opt back into
            // disk persistence by setting `checkpoints.log_path` in
            // the user config — there is no automatic rotation, so a
            // long session will grow the file without bound.
            log_path: None,
            recent_for_summary: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Directory where per-Claude-session Aura state files live.
    pub dir: PathBuf,
    /// Maximum chars of speech-safe recap to splice into Grok
    /// `instructions` on resume.
    pub max_recap_chars: usize,
    /// How many recent checkpoint speech lines to snapshot into the
    /// session for resume-time recap building.
    pub checkpoints_in_recap: usize,
    /// Drop session files idle longer than this on startup. `0` disables
    /// age-based pruning. Pruning of sessions whose underlying transcript
    /// is gone runs unconditionally.
    pub prune_after_days: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(".aura/sessions"),
            max_recap_chars: 1000,
            checkpoints_in_recap: 5,
            prune_after_days: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    /// Master switch. When false, `aura discord listen` refuses to start.
    pub enabled: bool,
    /// Name of the environment variable holding the Discord bot token.
    /// Mirrors the `XAI_API_KEY` pattern: secrets never live in config.
    pub bot_token_env: String,
    /// Discord user id the bot is allowed to DM. Single-user lock — the
    /// bot only contacts this one user, no broadcast.
    pub authorized_user_id: Option<u64>,
    /// Voice channel the bot joins on Ready. Set together with `guild_id`
    /// to enable voice; leave both `None` to keep the bot in DM-only mode.
    pub voice_channel_id: Option<u64>,
    /// Guild (server) the voice channel belongs to. Required when
    /// `voice_channel_id` is set.
    pub guild_id: Option<u64>,
    /// When true, the listener DMs the user on the
    /// `aura-needs-user-input.json` hook. Same trigger PR 1 wired into
    /// the live loop's Grok nudge.
    pub ping_on_needs_input: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token_env: "DISCORD_BOT_TOKEN".to_owned(),
            authorized_user_id: None,
            voice_channel_id: None,
            guild_id: None,
            ping_on_needs_input: true,
        }
    }
}

fn default_speech_rate() -> f64 {
    0.6
}
fn default_speech_volume() -> f64 {
    0.75
}
fn default_input_gain() -> f64 {
    0.5
}
fn default_compaction_threshold() -> f64 {
    0.9
}

/// Noise-suppression preset for the audio pipeline.
///
/// Serialized as snake_case strings so `.aura/config.json` keys match
/// the Swift UI's `noise_suppression` setting written by `SetSetting`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseSuppression {
    Off,
    Soft,
    #[default]
    Medium,
    Strong,
}

/// Per-microphone / speaker settings for the voice pipeline.
///
/// Written by the Swift settings UI via the `voice.*` allow-list in
/// `aura-cli::ui_action::validate_setting`. All fields carry
/// `#[serde(default)]` at the struct level so a partial JSON section
/// (e.g. only `{"voice":{"speech_rate":0.5}}`) merges with defaults
/// for every key that was omitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Enable always-on microphone capture for keyword detection.
    pub hot_mic: bool,
    /// Wake-word / phrase that activates voice mode.
    pub wake_phrase: String,
    /// TTS playback speed, normalised to 0.0–1.0.
    /// Clamped on deserialize (see `deserialize_clamped_01`).
    #[serde(
        default = "default_speech_rate",
        deserialize_with = "serde_clamped_01::deserialize"
    )]
    pub speech_rate: f64,
    /// TTS output volume, normalised to 0.0–1.0.
    /// Clamped on deserialize (see `deserialize_clamped_01`).
    #[serde(
        default = "default_speech_volume",
        deserialize_with = "serde_clamped_01::deserialize"
    )]
    pub speech_volume: f64,
    /// Allow the user to interrupt Aura mid-speech.
    pub barge_in: bool,
    /// Silence window (ms) after which Aura treats the turn as complete.
    pub end_of_turn_timeout_ms: u64,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            hot_mic: true,
            wake_phrase: "Aura".to_owned(),
            speech_rate: 0.6,
            speech_volume: 0.75,
            barge_in: true,
            end_of_turn_timeout_ms: 700,
        }
    }
}

/// Audio hardware routing and signal-processing options.
///
/// Written by the Swift settings UI via the `audio.*` allow-list.
/// `#[serde(default)]` at the struct level means partial sections in
/// user JSON still work — only supplied keys are overridden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Override the cpal default capture device. `None` → use cpal's
    /// system default. Set to a non-empty string to hard-pin a device
    /// (useful with USB interfaces such as "Scarlett 2i2 USB").
    pub input_device: Option<String>,
    /// Override the cpal default playback device. `None` → use cpal's
    /// system default.
    pub output_device: Option<String>,
    /// Pre-amplification factor for the capture stream, 0.0–1.0.
    /// Clamped on deserialize (see `deserialize_clamped_01`).
    #[serde(
        default = "default_input_gain",
        deserialize_with = "serde_clamped_01::deserialize"
    )]
    pub input_gain: f64,
    /// Noise-suppression preset applied to the capture stream.
    pub noise_suppression: NoiseSuppression,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            input_gain: 0.5,
            noise_suppression: NoiseSuppression::Medium,
        }
    }
}

/// Clamp `v` to `0.0..=1.0` during JSON deserialisation.
///
/// We clamp rather than reject because:
/// - The CLI surface (`ui_action::validate_setting`) already rejects
///   out-of-range values before they reach disk, so the "on-disk value
///   is authoritative" path only produces out-of-range data from manual
///   edits — and a silent clamp is a better recovery than a boot failure.
/// - Clamping is consistent with the pattern already used for
///   `codex.hot_interval_ms` / `claude.hot_interval_ms` in
///   `apply_codex_hot_interval_override`.
fn deserialize_clamped_01<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = f64::deserialize(deserializer)?;
    Ok(v.clamp(0.0, 1.0))
}

/// `serde` helper: deserialize an `f64` that is clamped to `0.0..=1.0`,
/// then wrap it in `Ok(Some(...))` so it can be used as
/// `#[serde(deserialize_with = ...)]` on `Option<f64>` fields if needed
/// in the future. Currently used on plain `f64` fields in
/// `VoiceConfig` and `AudioConfig`.
///
/// This module satisfies serde's `deserialize_with` attribute contract.
mod serde_clamped_01 {
    use serde::Deserializer;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<f64, D::Error>
    where
        D: Deserializer<'de>,
    {
        super::deserialize_clamped_01(deserializer)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuraConfig {
    /// Preferred edit-capable worker for this project (`codex` or
    /// `claude` today). Swift treats this top-level key as product
    /// state, so Rust owns it explicitly instead of relying on serde
    /// unknown-field tolerance to avoid erasing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Project folder bound to this `.aura` directory. Kept optional
    /// because older configs do not have it and install/test flows may
    /// infer the project from the config location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    pub callback_mode: CallbackMode,
    pub hush_mode: bool,
    pub provider: ProviderConfig,
    pub safety: SafetyConfig,
    pub history: HistoryConfig,
    pub claude: ClaudeConfig,
    pub codex: CodexConfig,
    pub checkpoints: CheckpointConfig,
    pub sessions: SessionConfig,
    pub discord: DiscordConfig,
    pub bridge: BridgeConfig,
    /// Floating-bar UI state owned by the Swift frontend. The daemon does
    /// not act on `bar.enabled`, but the field is owned explicitly so the
    /// dotted-key write path (`ui_action::set_setting` admits `bar.enabled`)
    /// round-trips through disk instead of being silently dropped on the
    /// next config save. Swift decodes this as `AuraSettingsSnapshot.barEnabled`.
    pub bar: BarConfig,
    /// Connection profiles for the multi-agent control panel (Codex /
    /// Claude Code / OpenClaw / Hermes). Follows the `bar` nested-struct
    /// pattern: `#[serde(default, skip_serializing_if = ...)]` so an
    /// existing config with no `connections` section round-trips with
    /// the field absent. The secret token is NEVER stored here — it
    /// lives in the macOS Keychain keyed by `identity`; this section
    /// holds only non-secret profile metadata.
    #[serde(default, skip_serializing_if = "ConnectionProfilesConfig::is_empty")]
    pub connections: ConnectionProfilesConfig,
    /// Voice-pipeline settings (hot-mic, wake phrase, TTS rate/volume, etc.).
    pub voice: VoiceConfig,
    /// Audio hardware routing and capture signal processing.
    pub audio: AudioConfig,
    /// Mirror of the `--debug` CLI flag, but config-level so the Swift
    /// UI can toggle verbose logging without relaunching with a flag.
    pub debug: bool,
    /// Whether the ambient context feeder is active.
    /// Maps to the "Ambient Context" toggle in the Swift settings UI.
    pub ambient_context: bool,
    /// Threshold (0.0–1.0) at which the context compaction strategy
    /// triggers. Clamped to `0.0..=1.0` on deserialize; see
    /// `deserialize_clamped_01` for the rationale.
    #[serde(
        default = "default_compaction_threshold",
        deserialize_with = "serde_clamped_01::deserialize"
    )]
    pub compaction_threshold: f64,
}

/// V1.5 autonomous callback via Claude Code Cloud Bridge.
///
/// When `auto_pickup` is enabled, after the voice client writes a
/// pending task to `<state_dir>/pending/<id>.json`, it ALSO POSTs the
/// `[aura-pickup <id>]` sentinel to the Anthropic cloud bridge for the
/// active Claude Code chat session. The bridge syncs the synthetic
/// user message back to the local CLI subprocess, where the V1 plugin's
/// UserPromptSubmit hook expands it and Claude processes the task —
/// all without the developer typing /aura-pickup.
///
/// Preconditions (all must hold or auto_pickup degrades to V1 manual):
/// 1. User has logged in to Claude Code with a Claude.ai subscription
///    (OAuth token in macOS Keychain `Claude Code-credentials`).
/// 2. The current Claude Code chat session has been activated for
///    remote-control via the `/remote-control` slash command. Without
///    this, no cse_* cloud session exists for the chat and the POST
///    target is unresolvable.
/// 3. The Anthropic cloud session API is reachable and returns 200
///    on the events POST.
///
/// On any precondition failure, the voice client logs a clear reason
/// and falls back to V1's manual `/aura-pickup` flow (the pending file
/// is still written, the user can pick it up by typing the slash
/// command). Honest fallback > silent degradation.
///
/// Default is OFF for V1.5 ship. Users opt-in by editing
/// `.aura/config.json` (or via `aura bridge enable` if/when we add
/// that subcommand). The opt-in default is the right posture because
/// a) the bridge POSTs as the user, which is sensitive, and b) the
/// /remote-control prerequisite isn't always done.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeConfig {
    pub auto_pickup: bool,
}

/// Floating-bar UI preference. Persisted purely so the Swift frontend's
/// `bar.enabled` toggle survives a config round-trip; the Rust daemon
/// itself does not consume `enabled`. Defaults to `true` to match the
/// `restore_settings` default in `ui_action.rs` (`bar.enabled => true`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    pub enabled: bool,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Connection profiles for the multi-agent control panel, stored in
/// `AuraConfig.connections`. Follows the `BarConfig` nested-struct
/// pattern. `is_empty()` powers the field's `skip_serializing_if` so a
/// config that has never declared a profile keeps the `connections`
/// key absent on disk (clean round-trip for legacy configs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionProfilesConfig {
    /// Ordered list of connection profiles for the 4-agent control panel.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ConnectionProfile>,
}

impl ConnectionProfilesConfig {
    /// True when no profiles are declared. Used as the
    /// `skip_serializing_if` predicate on `AuraConfig.connections` so
    /// the section is omitted entirely from serialized output when empty.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// One agent's non-secret connection metadata. The matching secret
/// token lives in the macOS Keychain (`AuraProviderKeychain`) keyed by
/// `identity` — it is never serialized here.
///
/// JSON keys are snake_case and match the control-center contract:
/// `kind`, `transport`, `endpoint`, `relay`, `identity`, `host_kind`,
/// `scopes`, `health`. Only `kind` and `transport` are required; every
/// other field has a serde default so partial profiles deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    /// Agent kind slug: `codex` | `claude-code` | `openclaw` | `hermes`.
    pub kind: String,
    /// How Aura reaches this agent's runtime.
    pub transport: AgentTransport,
    /// Direct endpoint (`https://host:port`); present when
    /// `transport == direct`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Relay URL (`wss://…/runtime-inbox/connect`); present when
    /// `transport == relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Token subject (`sub`); the Keychain key for this profile's
    /// secret token. The token itself is NOT stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Where the runtime lives: `local-mac` | `remote-mac` |
    /// `ci-runner` | `unknown`. Defaults to `"unknown"`.
    #[serde(default = "default_host_kind")]
    pub host_kind: String,
    /// Granted scopes for this profile's token, e.g. `["state:read"]`.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Last-known connection state: `connected` | `stale` |
    /// `unreachable` | `unknown`. Non-authoritative display hint
    /// (the runtime's `agent_health` is authoritative). Defaults to
    /// `"unknown"`.
    #[serde(default = "default_health")]
    pub health: String,
}

fn default_host_kind() -> String {
    "unknown".to_owned()
}

fn default_health() -> String {
    "unknown".to_owned()
}

impl Default for AuraConfig {
    fn default() -> Self {
        Self {
            worker: None,
            project_root: None,
            callback_mode: CallbackMode::PingFirst,
            hush_mode: false,
            provider: ProviderConfig::default(),
            safety: SafetyConfig::default(),
            history: HistoryConfig::default(),
            claude: ClaudeConfig::default(),
            codex: CodexConfig::default(),
            checkpoints: CheckpointConfig::default(),
            sessions: SessionConfig::default(),
            discord: DiscordConfig::default(),
            bridge: BridgeConfig::default(),
            bar: BarConfig::default(),
            connections: ConnectionProfilesConfig::default(),
            voice: VoiceConfig::default(),
            audio: AudioConfig::default(),
            debug: false,
            ambient_context: true,
            compaction_threshold: 0.9,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("AURA_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".aura/config.json"))
}

/// Lower bound for `codex.hot_interval_ms`. Below this, the feeder
/// pegs CPU and the app-server can't keep up; the runtime already
/// clamps to 500 in `aura-codex` (`Duration::from_millis(.max(500))`),
/// so we mirror the floor here for parity.
pub const HOT_INTERVAL_FLOOR_MS: u64 = 500;
/// Upper bound for `codex.hot_interval_ms`. Above this, the feeder
/// is so slow it stops being "hot" — anything in this regime should
/// turn the feeder off entirely. 10s gives a generous "very calm"
/// preset without permitting accidentally-disabled feeders.
pub const HOT_INTERVAL_CEIL_MS: u64 = 10_000;

/// Apply environment-variable overrides to the loaded config. Today
/// covers the per-agent `hot_interval_ms` knob via two parallel
/// env vars:
///
/// - `AURA_CODEX_HOT_INTERVAL_MS` → `config.codex.hot_interval_ms`
/// - `AURA_CLAUDE_HOT_INTERVAL_MS` → `config.claude.hot_interval_ms`
///
/// Both share the same `[HOT_INTERVAL_FLOOR_MS, HOT_INTERVAL_CEIL_MS]`
/// clamp window because both feeders enforce the same 500ms floor at
/// runtime and a 10s ceiling stops being "hot" for either agent.
/// Invalid / out-of-range values are clamped (we do not fail the
/// config load — a typo in an env var should not brick voice).
fn apply_env_overrides(mut config: AuraConfig) -> AuraConfig {
    let raw_codex_hot_interval = std::env::var("AURA_CODEX_HOT_INTERVAL_MS").ok();
    apply_codex_hot_interval_override(&mut config.codex, raw_codex_hot_interval.as_deref());
    let raw_claude_hot_interval = std::env::var("AURA_CLAUDE_HOT_INTERVAL_MS").ok();
    apply_claude_hot_interval_override(&mut config.claude, raw_claude_hot_interval.as_deref());
    config
}

/// Pure override step for `codex.hot_interval_ms`. Split from
/// `apply_env_overrides` so tests can exercise the parse/clamp logic
/// without mutating the process env (env mutation is `unsafe` in
/// Rust 1.78+ and racey across parallel test threads).
///
/// - `None` → leave existing value untouched.
/// - `Some(invalid)` → leave existing value untouched (a typo
///   should not brick voice; we silently fall back).
/// - `Some(valid)` → clamp to `[HOT_INTERVAL_FLOOR_MS,
///   HOT_INTERVAL_CEIL_MS]` and replace.
pub fn apply_codex_hot_interval_override(codex: &mut CodexConfig, raw: Option<&str>) {
    if let Some(raw) = raw {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            codex.hot_interval_ms = parsed.clamp(HOT_INTERVAL_FLOOR_MS, HOT_INTERVAL_CEIL_MS);
        }
    }
}

/// Pure override step for `claude.hot_interval_ms`. Mirrors
/// `apply_codex_hot_interval_override` semantics so the env override
/// behaviour is symmetric between agents — operators get the same
/// parse, clamp, fallback, and trim rules whether they're tuning
/// `AURA_CODEX_HOT_INTERVAL_MS` or `AURA_CLAUDE_HOT_INTERVAL_MS`.
///
/// - `None` → leave existing value untouched.
/// - `Some(invalid)` → leave existing value untouched (a typo
///   should not brick voice; we silently fall back).
/// - `Some(valid)` → clamp to `[HOT_INTERVAL_FLOOR_MS,
///   HOT_INTERVAL_CEIL_MS]` and replace.
pub fn apply_claude_hot_interval_override(claude: &mut ClaudeConfig, raw: Option<&str>) {
    if let Some(raw) = raw {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            claude.hot_interval_ms = parsed.clamp(HOT_INTERVAL_FLOOR_MS, HOT_INTERVAL_CEIL_MS);
        }
    }
}

pub fn load_or_default(path: Option<&Path>) -> Result<AuraConfig, String> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);
    let absolute_path = absolute_config_path(&path)?;
    let base_dir = config_base_dir(&absolute_path);
    let file = match fs::OpenOptions::new().read(true).open(&absolute_path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(apply_env_overrides(normalize_paths(
                AuraConfig::default(),
                &base_dir,
            )));
        }
        Err(err) => {
            return Err(format!(
                "failed to read config at {}: {err}",
                absolute_path.display()
            ));
        }
    };
    // Kernel-bounded read: same shape as `read_reason` and the other
    // local JSON loaders. The config dir is local but anything with the
    // user's uid can swap a multi-GB blob in; cap it at 256 KiB.
    let mut buf = Vec::with_capacity(8 * 1024);
    file.take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| {
            format!(
                "failed to read config at {}: {err}",
                absolute_path.display()
            )
        })?;
    if buf.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(format!(
            "config {} exceeds {} byte cap (corruption or attack); refusing to load",
            absolute_path.display(),
            MAX_CONFIG_FILE_BYTES
        ));
    }
    let raw = std::str::from_utf8(&buf).map_err(|err| {
        format!(
            "config {} is not valid UTF-8: {err}",
            absolute_path.display()
        )
    })?;
    let config = serde_json::from_str(raw)
        .map_err(|err| format!("invalid config at {}: {err}", absolute_path.display()))?;
    Ok(apply_env_overrides(normalize_paths(config, &base_dir)))
}

pub fn save_default_config(path: &Path) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(&AuraConfig::default())
        .map_err(|err| format!("failed to serialize default config: {err}"))?;
    write_private_truncated(path, &raw, "config")
}

fn absolute_config_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|err| format!("failed to read current directory: {err}"))
    }
}

fn config_base_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.file_name().and_then(|name| name.to_str()) == Some(".aura") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn normalize_paths(mut config: AuraConfig, base_dir: &Path) -> AuraConfig {
    let defaults = AuraConfig::default();
    config.project_root = config
        .project_root
        .map(|path| normalize_path(base_dir, path));
    config.history.path =
        normalize_state_path(base_dir, config.history.path, defaults.history.path);
    config.claude.transcript_path = config
        .claude
        .transcript_path
        .map(|path| normalize_path(base_dir, path));
    config.claude.transcripts_dir = config
        .claude
        .transcripts_dir
        .map(|path| normalize_path(base_dir, path));
    config.claude.hooks_dir = config
        .claude
        .hooks_dir
        .map(|path| normalize_path(base_dir, path));
    config.claude.cli_path = config
        .claude
        .cli_path
        .and_then(|path| trusted_config_executable(base_dir, path));
    if !config.codex.app_server_bin.as_os_str().is_empty() {
        config.codex.app_server_bin =
            trusted_config_executable(base_dir, config.codex.app_server_bin)
                .unwrap_or_else(|| CodexConfig::default().app_server_bin);
    }
    config.codex.session_path = normalize_state_path(
        base_dir,
        config.codex.session_path,
        defaults.codex.session_path,
    );
    config.checkpoints.log_path = config.checkpoints.log_path.map(|path| {
        normalize_state_path(
            base_dir,
            path,
            CheckpointConfig::default()
                .log_path
                .unwrap_or_else(|| PathBuf::from(".aura/checkpoints.jsonl")),
        )
    });
    config.sessions.dir =
        normalize_state_path(base_dir, config.sessions.dir, defaults.sessions.dir);
    config
}

fn normalize_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn normalize_state_path(base_dir: &Path, path: PathBuf, fallback: PathBuf) -> PathBuf {
    let normalized = lexical_normalize(&normalize_path(base_dir, path));
    let state_root = lexical_normalize(&base_dir.join(".aura"));
    if path_is_inside_lexical(&normalized, &state_root) {
        normalized
    } else {
        lexical_normalize(&normalize_path(base_dir, fallback))
    }
}

fn path_is_inside_lexical(path: &Path, base_dir: &Path) -> bool {
    path == base_dir || path.starts_with(base_dir)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

fn trusted_config_executable(base_dir: &Path, path: PathBuf) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return Some(path);
    }
    if path.components().count() == 1 && !path.is_absolute() {
        return trusted_bare_executable_name(&path).then_some(path);
    }
    if !path.is_absolute() {
        return None;
    }
    if path_is_inside(&path, base_dir) {
        return None;
    }
    is_trusted_executable_prefix(&path).then_some(path)
}

pub fn trusted_operator_executable(base_dir: &Path, path: PathBuf) -> Option<PathBuf> {
    trusted_config_executable(base_dir, path)
}

fn trusted_bare_executable_name(path: &Path) -> bool {
    matches!(path.to_str(), Some("claude" | "codex" | "codex-app-server"))
}

fn path_is_inside(path: &Path, base_dir: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());
    path.starts_with(base)
}

fn is_trusted_executable_prefix(path: &Path) -> bool {
    let trusted_roots = [
        Path::new("/bin"),
        Path::new("/usr/bin"),
        Path::new("/usr/local/bin"),
        Path::new("/opt/homebrew/bin"),
        Path::new("/usr/local/Cellar"),
        Path::new("/opt/homebrew/Cellar"),
    ];
    if trusted_roots.iter().any(|root| path.starts_with(root)) {
        return true;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    path.starts_with(home.join(".cargo").join("bin"))
        || path.starts_with(home.join(".local").join("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_callback_aliases() {
        assert_eq!(
            "ping".parse::<CallbackMode>().unwrap(),
            CallbackMode::PingFirst
        );
        assert_eq!(
            "immediate".parse::<CallbackMode>().unwrap(),
            CallbackMode::SpeakImmediately
        );
        assert_eq!(
            "silent".parse::<CallbackMode>().unwrap(),
            CallbackMode::SilentNotification
        );
        // `hangup` / `auto_hangup` are aliases for PingFirst — the model
        // invents these when reasoning about async-handoff dispatch
        // (observed in production). Pinned here so a future
        // refactor can't silently drop them.
        assert_eq!(
            "hangup".parse::<CallbackMode>().unwrap(),
            CallbackMode::PingFirst
        );
        assert_eq!(
            "auto_hangup".parse::<CallbackMode>().unwrap(),
            CallbackMode::PingFirst
        );
        // Case-insensitive alias survives mixed-case shapes the model
        // might emit ('Hangup', 'AUTO_HANGUP', etc.).
        assert_eq!(
            "Hangup".parse::<CallbackMode>().unwrap(),
            CallbackMode::PingFirst
        );
    }

    #[test]
    fn default_config_has_v1_controls() {
        let config = AuraConfig::default();
        assert!(!config.hush_mode);
        assert_eq!(config.provider.latency_target_ms, 800);
        assert!(config.safety.local_only);
        assert!(config.safety.require_voice_approval);
        assert!(config.history.path.ends_with(".aura/history.jsonl"));
    }

    #[test]
    fn default_checkpoint_log_path_is_in_memory_only() {
        // The persisted log is append-only with no rotation; long
        // sessions would grow it without bound. The in-memory ring
        // is the durable surface across the lifetime of a single
        // live_call. Users opt into disk persistence explicitly.
        let cfg = CheckpointConfig::default();
        assert!(
            cfg.log_path.is_none(),
            "default must be in-memory only; got {:?}",
            cfg.log_path
        );
    }

    #[test]
    fn loads_config_from_json() {
        let raw = r#"{
          "worker": "claude",
          "project_root": ".",
          "callback_mode": "speak_immediately",
          "hush_mode": true,
          "provider": {"model":"grok-voice-think-fast-1.0","voice":"rex","latency_target_ms":700},
              "safety": {"local_only": true, "require_voice_approval": true, "require_cancel_confirmation": true},
          "history": {"path": ".aura/test-history.jsonl", "max_events": 50},
          "claude": {"transcript_path": "claude.jsonl", "hooks_dir": "hooks"}
        }"#;
        let config: AuraConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.worker.as_deref(), Some("claude"));
        assert_eq!(config.project_root.as_deref(), Some(Path::new(".")));
        assert_eq!(config.callback_mode, CallbackMode::SpeakImmediately);
        assert!(config.hush_mode);
        assert_eq!(config.provider.voice, "rex");
        assert_eq!(config.history.max_events, 50);
        assert_eq!(
            config.claude.transcript_path.unwrap(),
            PathBuf::from("claude.jsonl")
        );
    }

    #[test]
    fn load_resolves_relative_paths_against_project_root() {
        let dir = std::env::temp_dir().join(format!(
            "aura-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let aura_dir = dir.join(".aura");
        fs::create_dir_all(&aura_dir).unwrap();
        let config_path = aura_dir.join("config.json");
        fs::write(
            &config_path,
            r#"{
              "worker": "codex",
              "project_root": ".",
              "callback_mode": "ping_first",
              "provider": {"model":"grok-voice-think-fast-1.0","voice":"eve","latency_target_ms":800},
              "safety": {"local_only": true, "require_voice_approval": true, "require_cancel_confirmation": true},
              "history": {"path": ".aura/history.jsonl", "max_events": 50},
              "claude": {"transcript_path": "claude.jsonl", "hooks_dir": "hooks"}
            }"#,
        )
        .unwrap();

        let config = load_or_default(Some(&config_path)).unwrap();
        assert_eq!(config.worker.as_deref(), Some("codex"));
        assert_eq!(config.project_root.as_deref(), Some(dir.as_path()));
        assert_eq!(config.history.path, dir.join(".aura/history.jsonl"));
        assert_eq!(
            config.claude.transcript_path.unwrap(),
            dir.join("claude.jsonl")
        );
        assert_eq!(config.claude.hooks_dir.unwrap(), dir.join("hooks"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_keeps_swift_owned_project_binding_fields_optional_and_tolerant() {
        let raw = r#"{
          "worker": "claude",
          "project_root": "/tmp/aura-project",
          "future_swift_only_field": { "keep": true }
        }"#;
        let config: AuraConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(config.worker.as_deref(), Some("claude"));
        assert_eq!(
            config.project_root.as_deref(),
            Some(Path::new("/tmp/aura-project"))
        );

        let defaults = serde_json::to_value(AuraConfig::default()).unwrap();
        assert!(
            defaults.get("worker").is_none(),
            "default configs should not write null worker"
        );
        assert!(
            defaults.get("project_root").is_none(),
            "default configs should not write null project_root"
        );
    }

    #[test]
    fn load_confines_writable_state_paths_to_aura_dir() {
        let dir = std::env::temp_dir().join(format!(
            "aura-config-state-confine-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let aura_dir = dir.join(".aura");
        fs::create_dir_all(&aura_dir).unwrap();
        let config_path = aura_dir.join("config.json");
        fs::write(
            &config_path,
            format!(
                r#"{{
                  "history": {{"path": "{}/outside-history.jsonl", "max_events": 50}},
                  "codex": {{"session_path": "../session.json"}},
                  "checkpoints": {{"log_path": "{}"}},
                  "sessions": {{"dir": ".aura/../sessions"}}
                }}"#,
                std::env::temp_dir().display(),
                dir.join("not-aura").join("checkpoints.jsonl").display()
            ),
        )
        .unwrap();

        let config = load_or_default(Some(&config_path)).unwrap();

        assert_eq!(config.history.path, dir.join(".aura/history.jsonl"));
        assert_eq!(
            config.codex.session_path,
            dir.join(".aura/codex/session.json")
        );
        assert_eq!(
            config.checkpoints.log_path,
            Some(dir.join(".aura/checkpoints.jsonl"))
        );
        assert_eq!(config.sessions.dir, dir.join(".aura/sessions"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_ignores_project_local_executable_paths() {
        let dir = std::env::temp_dir().join(format!(
            "aura-config-exec-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let aura_dir = dir.join(".aura");
        fs::create_dir_all(&aura_dir).unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        let config_path = aura_dir.join("config.json");
        fs::write(
            &config_path,
            r#"{
              "claude": {"cli_path": "bin/fake-claude"},
              "codex": {"app_server_bin": "bin/fake-codex"}
            }"#,
        )
        .unwrap();

        let config = load_or_default(Some(&config_path)).unwrap();

        assert_eq!(config.claude.cli_path, None);
        assert_eq!(config.codex.app_server_bin, PathBuf::from("codex"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_accepts_only_known_bare_executable_names() {
        let dir = std::env::temp_dir().join(format!(
            "aura-config-bare-exec-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let aura_dir = dir.join(".aura");
        fs::create_dir_all(&aura_dir).unwrap();
        let config_path = aura_dir.join("config.json");
        fs::write(
            &config_path,
            r#"{
              "claude": {"cli_path": "claude"},
              "codex": {"app_server_bin": "evil-codex"}
            }"#,
        )
        .unwrap();

        let config = load_or_default(Some(&config_path)).unwrap();

        assert_eq!(config.claude.cli_path, Some(PathBuf::from("claude")));
        assert_eq!(config.codex.app_server_bin, PathBuf::from("codex"));
        assert!(trusted_operator_executable(&dir, PathBuf::from("evil-claude")).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    // V2 step: hot-interval knob in config + Voice tab. The override
    // logic is split from the env-var read so we can pin parse + clamp
    // behavior without `unsafe` env mutation in parallel tests.
    #[test]
    fn hot_interval_override_replaces_default_when_in_range() {
        let mut codex = CodexConfig::default();
        assert_eq!(codex.hot_interval_ms, 1500);
        apply_codex_hot_interval_override(&mut codex, Some("2500"));
        assert_eq!(codex.hot_interval_ms, 2500);
    }

    #[test]
    fn hot_interval_override_clamps_below_floor() {
        // Under the 500ms floor the feeder pegs CPU; the runtime
        // already enforces this in `aura-codex`, but the config
        // surface should not even round-trip a sub-floor value.
        let mut codex = CodexConfig::default();
        apply_codex_hot_interval_override(&mut codex, Some("100"));
        assert_eq!(codex.hot_interval_ms, HOT_INTERVAL_FLOOR_MS);
    }

    #[test]
    fn hot_interval_override_clamps_above_ceil() {
        // Above 10s the feeder isn't really "hot" any more — clamp
        // to ceil rather than silently turning the feeder into a
        // disabled state operators don't expect.
        let mut codex = CodexConfig::default();
        apply_codex_hot_interval_override(&mut codex, Some("60000"));
        assert_eq!(codex.hot_interval_ms, HOT_INTERVAL_CEIL_MS);
    }

    #[test]
    fn hot_interval_override_ignores_invalid_input() {
        // Typos / non-numeric values fall back to the loaded value so
        // a stray export of `AURA_CODEX_HOT_INTERVAL_MS=fast` doesn't
        // brick voice.
        let mut codex = CodexConfig {
            hot_interval_ms: 2000,
            ..CodexConfig::default()
        };
        apply_codex_hot_interval_override(&mut codex, Some("fast"));
        assert_eq!(codex.hot_interval_ms, 2000);
        apply_codex_hot_interval_override(&mut codex, Some(""));
        assert_eq!(codex.hot_interval_ms, 2000);
    }

    #[test]
    fn hot_interval_override_none_is_a_noop() {
        // Absence of the env var must leave the loaded value alone —
        // this is the "user has not opted in" path.
        let mut codex = CodexConfig {
            hot_interval_ms: 1750,
            ..CodexConfig::default()
        };
        apply_codex_hot_interval_override(&mut codex, None);
        assert_eq!(codex.hot_interval_ms, 1750);
    }

    #[test]
    fn hot_interval_override_trims_whitespace() {
        // Shell exports often pick up trailing whitespace from
        // `.envrc` or interactive paste; trim before parsing rather
        // than failing.
        let mut codex = CodexConfig::default();
        apply_codex_hot_interval_override(&mut codex, Some("  3000  "));
        assert_eq!(codex.hot_interval_ms, 3000);
    }

    // V2 parity step: ClaudeConfig gained 6 feeder fields
    // (hot_interval_ms, feeder_mode, hot_model, research_model,
    // research_max_in_flight, search_fanout) so `--agent claude`
    // honours the same operator knobs as `--agent codex`. The
    // defaults must match the previously-hardcoded values inside
    // `aura-cli::feeder_setup` so on-disk configs behave consistently.
    #[test]
    fn claude_feeder_defaults_match_pre_parity_hardcodes() {
        let c = ClaudeConfig::default();
        // 3 seconds = the old `CycleConfig::default().interval`.
        assert_eq!(c.hot_interval_ms, 3000);
        // The two model strings used to be `"claude-sonnet-4-6"` literals
        // in `feeder_setup.rs`. Pin both so a future rename is caught.
        assert_eq!(c.hot_model, "claude-sonnet-4-6");
        assert_eq!(c.research_model, "claude-sonnet-4-6");
        // The research supervisor used to take `max_in_flight: 3`
        // hardcoded. Default kept at 3 for backward compat — Codex
        // intentionally diverges (default 2). Documented in the
        // field-level doc comment.
        assert_eq!(c.research_max_in_flight, 3);
        // `feeder_mode` and `search_fanout` are reserved-for-now;
        // pin their defaults so JSON round-trips don't drift.
        assert_eq!(c.feeder_mode, "balanced");
        assert_eq!(c.search_fanout, 8);
    }

    #[test]
    fn claude_feeder_fields_round_trip_through_json() {
        // Operators expect to override every knob via TOML/JSON.
        // The serde `#[serde(default)]` on the struct means missing
        // keys keep the default; supplied keys must round-trip.
        let raw = r#"{
          "transcript_path": "claude.jsonl",
          "hot_interval_ms": 1750,
          "feeder_mode": "aggressive",
          "hot_model": "claude-sonnet-4-7",
          "research_model": "claude-haiku-4-7",
          "research_max_in_flight": 2,
          "search_fanout": 12
        }"#;
        let c: ClaudeConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(c.hot_interval_ms, 1750);
        assert_eq!(c.feeder_mode, "aggressive");
        assert_eq!(c.hot_model, "claude-sonnet-4-7");
        assert_eq!(c.research_model, "claude-haiku-4-7");
        assert_eq!(c.research_max_in_flight, 2);
        assert_eq!(c.search_fanout, 12);
        // Sanity: existing fields still parse alongside.
        assert_eq!(c.transcript_path.unwrap(), PathBuf::from("claude.jsonl"));
        // `permission_mode` was not supplied → default kicks in.
        assert_eq!(c.permission_mode, "acceptEdits");
    }

    #[test]
    fn bar_enabled_round_trips_through_aura_config_json() {
        // Regression for the orphaned `bar.enabled` key: the Swift UI
        // writes `{"bar":{"enabled":false}}` via `ui_action::set_setting`,
        // so the daemon must deserialize + re-serialize it instead of
        // dropping it on the next save.
        assert!(BarConfig::default().enabled, "bar defaults to enabled");

        // Explicit `false` survives a full config round-trip.
        let raw = r#"{ "bar": { "enabled": false } }"#;
        let cfg: AuraConfig = serde_json::from_str(raw).unwrap();
        assert!(!cfg.bar.enabled);
        let reserialized = serde_json::to_string(&cfg).unwrap();
        let reparsed: AuraConfig = serde_json::from_str(&reserialized).unwrap();
        assert!(!reparsed.bar.enabled, "bar.enabled must not be dropped");

        // A config without `bar` keeps the default (true).
        let cfg_missing: AuraConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg_missing.bar.enabled);
    }

    // --- R10: Claude hot-interval env override (symmetry with R4) ---

    #[test]
    fn claude_hot_interval_override_replaces_default_when_in_range() {
        let mut claude = ClaudeConfig::default();
        assert_eq!(claude.hot_interval_ms, 3000);
        apply_claude_hot_interval_override(&mut claude, Some("2500"));
        assert_eq!(claude.hot_interval_ms, 2500);
    }

    #[test]
    fn claude_hot_interval_override_clamps_below_floor() {
        // Symmetric with the Codex floor — Claude's runtime clamp
        // also lives at 500ms (`CLAUDE_HOT_INTERVAL_FLOOR_MS` in
        // `aura-cli::feeder_setup`); the config surface mirrors it.
        let mut claude = ClaudeConfig::default();
        apply_claude_hot_interval_override(&mut claude, Some("100"));
        assert_eq!(claude.hot_interval_ms, HOT_INTERVAL_FLOOR_MS);
    }

    #[test]
    fn claude_hot_interval_override_clamps_above_ceil() {
        let mut claude = ClaudeConfig::default();
        apply_claude_hot_interval_override(&mut claude, Some("60000"));
        assert_eq!(claude.hot_interval_ms, HOT_INTERVAL_CEIL_MS);
    }

    #[test]
    fn claude_hot_interval_override_ignores_invalid_input() {
        // A stray export of `AURA_CLAUDE_HOT_INTERVAL_MS=fast` must
        // fall back to the loaded value rather than bricking voice.
        let mut claude = ClaudeConfig {
            hot_interval_ms: 2000,
            ..ClaudeConfig::default()
        };
        apply_claude_hot_interval_override(&mut claude, Some("fast"));
        assert_eq!(claude.hot_interval_ms, 2000);
        apply_claude_hot_interval_override(&mut claude, Some(""));
        assert_eq!(claude.hot_interval_ms, 2000);
    }

    #[test]
    fn claude_hot_interval_override_none_is_a_noop() {
        let mut claude = ClaudeConfig {
            hot_interval_ms: 1750,
            ..ClaudeConfig::default()
        };
        apply_claude_hot_interval_override(&mut claude, None);
        assert_eq!(claude.hot_interval_ms, 1750);
    }

    #[test]
    fn claude_hot_interval_override_trims_whitespace() {
        let mut claude = ClaudeConfig::default();
        apply_claude_hot_interval_override(&mut claude, Some("  3000  "));
        assert_eq!(claude.hot_interval_ms, 3000);
    }

    /// Symmetry property: for valid inputs, both override helpers
    /// must produce the same numeric result. A future refactor that
    /// diverges the parse/clamp logic silently breaks operator
    /// expectations ("AURA_*_HOT_INTERVAL_MS=2500 should mean 2500ms
    /// regardless of agent"). Invalid inputs intentionally fall back
    /// to their own per-agent defaults, so they're excluded here.
    #[test]
    fn codex_and_claude_overrides_are_symmetric_for_valid_inputs() {
        for raw in ["750", "  1500  ", "2500", "60000", "100"] {
            let mut codex = CodexConfig::default();
            let mut claude = ClaudeConfig::default();
            apply_codex_hot_interval_override(&mut codex, Some(raw));
            apply_claude_hot_interval_override(&mut claude, Some(raw));
            assert_eq!(
                codex.hot_interval_ms, claude.hot_interval_ms,
                "override for {raw:?} must produce the same value on both agents"
            );
        }
    }

    #[test]
    fn claude_feeder_fields_can_be_omitted_for_back_compat() {
        // Pre-parity configs (no Claude feeder block) must keep
        // working — `#[serde(default)]` fills the new fields with
        // the pre-parity hardcodes.
        let raw = r#"{
          "transcript_path": "old.jsonl",
          "hooks_dir": "hooks",
          "execute_tasks": true
        }"#;
        let c: ClaudeConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(c.hot_interval_ms, 3000);
        assert_eq!(c.hot_model, "claude-sonnet-4-6");
        assert_eq!(c.research_max_in_flight, 3);
        // And the supplied fields landed.
        assert!(c.execute_tasks);
        assert_eq!(c.hooks_dir.unwrap(), PathBuf::from("hooks"));
    }

    // --- Swift Settings UI: new voice / audio / debug fields ---

    /// Confirm the documented first-launch defaults for every new field.
    /// These values are also the Swift UI's initial-state fallbacks, so
    /// changing them here is a breaking change for the settings panel.
    #[test]
    fn default_config_has_swift_ui_defaults() {
        let config = AuraConfig::default();
        // Top-level
        assert!(!config.debug);
        assert!(config.ambient_context);
        assert_eq!(config.compaction_threshold, 0.9);
        // VoiceConfig
        assert!(config.voice.hot_mic);
        assert_eq!(config.voice.wake_phrase, "Aura");
        assert_eq!(config.voice.speech_rate, 0.6);
        assert_eq!(config.voice.speech_volume, 0.75);
        assert!(config.voice.barge_in);
        assert_eq!(config.voice.end_of_turn_timeout_ms, 700);
        // AudioConfig
        assert_eq!(config.audio.input_device, None);
        assert_eq!(config.audio.output_device, None);
        assert_eq!(config.audio.input_gain, 0.5);
        assert_eq!(config.audio.noise_suppression, NoiseSuppression::Medium);
    }

    /// Round-trip a fully-populated JSON including all new fields.
    #[test]
    fn fully_populated_json_round_trips() {
        let raw = r#"{
          "debug": true,
          "ambient_context": false,
          "compaction_threshold": 0.7,
          "voice": {
            "hot_mic": false,
            "wake_phrase": "Ravshan",
            "speech_rate": 0.4,
            "speech_volume": 0.9,
            "barge_in": false,
            "end_of_turn_timeout_ms": 1200
          },
          "audio": {
            "input_device": "Scarlett 2i2 USB",
            "output_device": "AirPods Pro",
            "input_gain": 0.8,
            "noise_suppression": "strong"
          }
        }"#;
        let config: AuraConfig = serde_json::from_str(raw).unwrap();
        assert!(config.debug);
        assert!(!config.ambient_context);
        assert_eq!(config.compaction_threshold, 0.7);
        // voice
        assert!(!config.voice.hot_mic);
        assert_eq!(config.voice.wake_phrase, "Ravshan");
        assert_eq!(config.voice.speech_rate, 0.4);
        assert_eq!(config.voice.speech_volume, 0.9);
        assert!(!config.voice.barge_in);
        assert_eq!(config.voice.end_of_turn_timeout_ms, 1200);
        // audio
        assert_eq!(
            config.audio.input_device.as_deref(),
            Some("Scarlett 2i2 USB")
        );
        assert_eq!(config.audio.output_device.as_deref(), Some("AirPods Pro"));
        assert_eq!(config.audio.input_gain, 0.8);
        assert_eq!(config.audio.noise_suppression, NoiseSuppression::Strong);
    }

    /// A partial JSON for the `voice` section must fill every omitted
    /// field with its documented default. This validates `#[serde(default)]`
    /// on `VoiceConfig` (and transitively on `AuraConfig`).
    #[test]
    fn partial_voice_section_fills_missing_fields_with_defaults() {
        let raw = r#"{"voice": {"speech_rate": 0.5}}"#;
        let config: AuraConfig = serde_json::from_str(raw).unwrap();
        // The supplied field landed.
        assert_eq!(config.voice.speech_rate, 0.5);
        // All other VoiceConfig fields take their defaults.
        assert!(config.voice.hot_mic);
        assert_eq!(config.voice.wake_phrase, "Aura");
        assert_eq!(config.voice.speech_volume, 0.75);
        assert!(config.voice.barge_in);
        assert_eq!(config.voice.end_of_turn_timeout_ms, 700);
        // Top-level new fields also get their defaults.
        assert!(!config.debug);
        assert!(config.ambient_context);
        assert_eq!(config.compaction_threshold, 0.9);
    }

    /// An out-of-range `compaction_threshold` (e.g. 1.5) is silently
    /// clamped to 1.0. We clamp rather than reject because:
    /// (a) the CLI surface already rejects out-of-range before writing,
    /// (b) clamping matches the pattern used for `hot_interval_ms`, and
    /// (c) a boot failure from a manual config edit is worse UX than a
    /// clamped value.
    #[test]
    fn compaction_threshold_out_of_range_is_clamped() {
        // Over the upper bound → clamped to 1.0.
        let high: AuraConfig = serde_json::from_str(r#"{"compaction_threshold": 1.5}"#).unwrap();
        assert_eq!(high.compaction_threshold, 1.0);
        // Under the lower bound → clamped to 0.0.
        let low: AuraConfig = serde_json::from_str(r#"{"compaction_threshold": -0.1}"#).unwrap();
        assert_eq!(low.compaction_threshold, 0.0);
        // In-range value is preserved exactly.
        let ok: AuraConfig = serde_json::from_str(r#"{"compaction_threshold": 0.7}"#).unwrap();
        assert_eq!(ok.compaction_threshold, 0.7);
    }

    #[test]
    fn voice_engine_defaults_to_grok() {
        let config = AuraConfig::default();
        assert_eq!(config.provider.engine, VoiceEngine::Grok);
        // Canonical on-disk form is the explicit `_realtime` suffix
        // so the file is in lockstep with what `aura-orb` writes.
        assert_eq!(config.provider.engine.as_str(), "grok_realtime");
    }

    #[test]
    fn voice_engine_accepts_canonical_realtime_names() {
        // Long form is canonical (matches aura-orb's NSPopUpButton).
        let grok: ProviderConfig = serde_json::from_str(
            r#"{"engine":"grok_realtime","model":"x","voice":"y","latency_target_ms":800}"#,
        )
        .expect("grok_realtime parses");
        assert_eq!(grok.engine, VoiceEngine::Grok);

        let openai: ProviderConfig = serde_json::from_str(
            r#"{"engine":"openai_realtime","model":"gpt-realtime-2","voice":"alloy","latency_target_ms":600}"#,
        )
        .expect("openai_realtime parses");
        assert_eq!(openai.engine, VoiceEngine::OpenAI);
        assert_eq!(openai.engine.as_str(), "openai_realtime");
    }

    #[test]
    fn effective_voice_picks_grok_voice_when_engine_is_grok() {
        let p = ProviderConfig {
            engine: VoiceEngine::Grok,
            grok_voice: Some("rex".to_owned()),
            openai_voice: Some("alloy".to_owned()),
            ..ProviderConfig::default()
        };
        assert_eq!(p.effective_voice(), "rex");
    }

    #[test]
    fn effective_voice_picks_openai_voice_when_engine_is_openai() {
        let p = ProviderConfig {
            engine: VoiceEngine::OpenAI,
            grok_voice: Some("rex".to_owned()),
            openai_voice: Some("alloy".to_owned()),
            ..ProviderConfig::default()
        };
        assert_eq!(p.effective_voice(), "alloy");
    }

    #[test]
    fn effective_voice_falls_back_to_legacy_voice_field() {
        // Older configs only have `provider.voice`. The runtime
        // should keep working until the user explicitly picks an
        // engine-specific voice.
        let p = ProviderConfig {
            engine: VoiceEngine::OpenAI,
            voice: "alloy".to_owned(),
            openai_voice: None,
            ..ProviderConfig::default()
        };
        assert_eq!(p.effective_voice(), "alloy");
    }

    #[test]
    fn effective_voice_defaults_to_sane_engine_voice_when_everything_missing() {
        // Pathological config — both legacy and engine-specific
        // fields empty. The runtime must still pick a working voice
        // name the provider knows about rather than crashing the WS
        // session.update with an empty voice string.
        let mut p = ProviderConfig {
            engine: VoiceEngine::OpenAI,
            voice: String::new(),
            openai_voice: None,
            ..ProviderConfig::default()
        };
        assert_eq!(p.effective_voice(), "alloy");

        p.engine = VoiceEngine::Grok;
        p.grok_voice = None;
        assert_eq!(p.effective_voice(), "eve");
    }

    #[test]
    fn effective_voice_ignores_grok_default_when_engine_is_openai() {
        let p: ProviderConfig = serde_json::from_str(r#"{"engine":"openai_realtime"}"#).unwrap();
        assert_eq!(p.voice, "eve");
        assert_eq!(p.effective_voice(), "alloy");
    }

    #[test]
    fn voice_engine_accepts_short_aliases() {
        // Aliases keep the CLI ergonomic: `set-setting provider.engine
        // "openai"` works as a shorthand. Serialize still emits the
        // canonical long form so the file stays consistent.
        let grok: ProviderConfig = serde_json::from_str(
            r#"{"engine":"grok","model":"x","voice":"y","latency_target_ms":800}"#,
        )
        .expect("grok alias parses");
        assert_eq!(grok.engine, VoiceEngine::Grok);

        let openai: ProviderConfig = serde_json::from_str(
            r#"{"engine":"openai","model":"x","voice":"y","latency_target_ms":800}"#,
        )
        .expect("openai alias parses");
        assert_eq!(openai.engine, VoiceEngine::OpenAI);

        // Round-trip canonicalises to the long form.
        let serialized = serde_json::to_value(openai.engine).unwrap();
        assert_eq!(serialized, serde_json::json!("openai_realtime"));
    }

    #[test]
    fn voice_engine_missing_field_falls_back_to_grok() {
        // Older configs predate the engine field. They should keep
        // working — provider.engine omitted means we stay on Grok,
        // which preserves existing behaviour.
        let legacy: ProviderConfig = serde_json::from_str(
            r#"{"model":"grok-voice-think-fast-1.0","voice":"eve","latency_target_ms":800}"#,
        )
        .expect("engine-less config parses");
        assert_eq!(legacy.engine, VoiceEngine::Grok);
    }

    #[test]
    fn partial_provider_section_fills_missing_fields_with_defaults() {
        let config: AuraConfig =
            serde_json::from_str(r#"{"provider":{"engine":"openai_realtime"}}"#)
                .expect("partial provider config parses");

        assert_eq!(config.provider.engine, VoiceEngine::OpenAI);
        assert_eq!(config.provider.model, "grok-voice-think-fast-1.0");
        assert_eq!(config.provider.voice, "eve");
        assert_eq!(config.provider.latency_target_ms, 800);
        assert_eq!(config.provider.temperature, Some(0.5));
    }

    #[test]
    fn provider_latency_target_defaults_for_legacy_configs() {
        let legacy: ProviderConfig = serde_json::from_str(
            r#"{"engine":"openai_realtime","model":"gpt-realtime-2","voice":"alloy"}"#,
        )
        .expect("legacy provider config parses without latency target");

        assert_eq!(legacy.latency_target_ms, 800);
    }

    #[test]
    fn voice_engine_rejects_unknown_values() {
        // Typo / hostile config should fail loudly at deserialize
        // time rather than silently mis-route the voice loop.
        let bad: Result<ProviderConfig, _> = serde_json::from_str(
            r#"{"engine":"claude","model":"x","voice":"y","latency_target_ms":800}"#,
        );
        assert!(bad.is_err(), "unknown engine value should be rejected");
    }

    // --- Multi-agent control center: AgentTransport / AgentHealth +
    //     connection-profiles config section (Contracts A and C). ---

    /// The shared transport/health enums are the wire vocabulary for
    /// `live-state.json`, `active-task.json`, and connection profiles.
    /// Their snake_case JSON values are a cross-crate + cross-language
    /// contract (Rust writer ↔ Swift reader), so pin every variant.
    #[test]
    fn agent_transport_and_health_serialize_snake_case() {
        assert_eq!(
            serde_json::to_value(AgentTransport::Local).unwrap(),
            serde_json::json!("local")
        );
        assert_eq!(
            serde_json::to_value(AgentTransport::Direct).unwrap(),
            serde_json::json!("direct")
        );
        assert_eq!(
            serde_json::to_value(AgentTransport::Relay).unwrap(),
            serde_json::json!("relay")
        );
        assert_eq!(
            serde_json::to_value(AgentHealth::Connected).unwrap(),
            serde_json::json!("connected")
        );
        assert_eq!(
            serde_json::to_value(AgentHealth::Stale).unwrap(),
            serde_json::json!("stale")
        );
        assert_eq!(
            serde_json::to_value(AgentHealth::Unreachable).unwrap(),
            serde_json::json!("unreachable")
        );

        // Round-trip the wire form back to the enum.
        assert_eq!(
            serde_json::from_value::<AgentTransport>(serde_json::json!("relay")).unwrap(),
            AgentTransport::Relay
        );
        assert_eq!(
            serde_json::from_value::<AgentHealth>(serde_json::json!("stale")).unwrap(),
            AgentHealth::Stale
        );
        // Unknown values are rejected (typo / hostile config fails loud).
        assert!(serde_json::from_value::<AgentTransport>(serde_json::json!("carrier")).is_err());
        assert!(serde_json::from_value::<AgentHealth>(serde_json::json!("flaky")).is_err());
    }

    /// A config with NO `connections` section must deserialize fine and
    /// — crucially — re-serialize WITHOUT emitting a `connections` key,
    /// so legacy configs round-trip byte-for-byte on the new schema.
    /// This is the backward-compatibility guarantee for the new section.
    #[test]
    fn config_without_connections_section_round_trips_without_emitting_it() {
        // The default config has no profiles, so `is_empty()` is true.
        assert!(ConnectionProfilesConfig::default().is_empty());

        // A config that omits `connections` parses, with an empty section.
        let cfg: AuraConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.connections.is_empty());
        assert!(cfg.connections.profiles.is_empty());

        // The default config must NOT serialize a `connections` key.
        let defaults = serde_json::to_value(AuraConfig::default()).unwrap();
        assert!(
            defaults.get("connections").is_none(),
            "default/empty connections must be skipped, not written as []"
        );

        // Full round-trip: parse → serialize → re-parse keeps it absent.
        let reserialized = serde_json::to_string(&cfg).unwrap();
        assert!(
            !reserialized.contains("connections"),
            "empty connections must not appear in serialized output"
        );
        let reparsed: AuraConfig = serde_json::from_str(&reserialized).unwrap();
        assert!(reparsed.connections.is_empty());
    }

    /// A config WITH one profile of each transport must round-trip,
    /// preserving every field. `kind` + `transport` are required; the
    /// other fields exercise both explicit values and serde defaults.
    #[test]
    fn config_with_one_profile_per_transport_round_trips() {
        let raw = r#"{
          "connections": {
            "profiles": [
              {
                "kind": "codex",
                "transport": "local"
              },
              {
                "kind": "claude-code",
                "transport": "direct",
                "endpoint": "https://mac-studio.local:8642",
                "identity": "claude-acc-abc",
                "host_kind": "remote-mac",
                "scopes": ["state:read", "dispatch:write"],
                "health": "connected"
              },
              {
                "kind": "hermes",
                "transport": "relay",
                "relay": "wss://api.codexini.com/runtime-inbox/connect",
                "identity": "hermes-sub-xyz",
                "scopes": ["state:read"]
              }
            ]
          }
        }"#;
        let cfg: AuraConfig = serde_json::from_str(raw).unwrap();
        assert!(!cfg.connections.is_empty());
        assert_eq!(cfg.connections.profiles.len(), 3);

        // Profile 0 — Local, everything-else-defaulted.
        let local = &cfg.connections.profiles[0];
        assert_eq!(local.kind, "codex");
        assert_eq!(local.transport, AgentTransport::Local);
        assert_eq!(local.endpoint, None);
        assert_eq!(local.relay, None);
        assert_eq!(local.identity, None);
        assert_eq!(local.host_kind, "unknown"); // default_host_kind
        assert!(local.scopes.is_empty());
        assert_eq!(local.health, "unknown"); // default_health

        // Profile 1 — Direct, all fields explicit.
        let direct = &cfg.connections.profiles[1];
        assert_eq!(direct.kind, "claude-code");
        assert_eq!(direct.transport, AgentTransport::Direct);
        assert_eq!(
            direct.endpoint.as_deref(),
            Some("https://mac-studio.local:8642")
        );
        assert_eq!(direct.identity.as_deref(), Some("claude-acc-abc"));
        assert_eq!(direct.host_kind, "remote-mac");
        assert_eq!(direct.scopes, vec!["state:read", "dispatch:write"]);
        assert_eq!(direct.health, "connected");

        // Profile 2 — Relay, host_kind/health defaulted.
        let relay = &cfg.connections.profiles[2];
        assert_eq!(relay.kind, "hermes");
        assert_eq!(relay.transport, AgentTransport::Relay);
        assert_eq!(
            relay.relay.as_deref(),
            Some("wss://api.codexini.com/runtime-inbox/connect")
        );
        assert_eq!(relay.identity.as_deref(), Some("hermes-sub-xyz"));
        assert_eq!(relay.host_kind, "unknown");
        assert_eq!(relay.scopes, vec!["state:read"]);
        assert_eq!(relay.health, "unknown");

        // Serialize → re-parse keeps all three profiles intact, and a
        // non-empty section IS emitted.
        let reserialized = serde_json::to_string(&cfg).unwrap();
        assert!(reserialized.contains("connections"));
        let reparsed: AuraConfig = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(reparsed.connections.profiles, cfg.connections.profiles);

        // A bad transport in a profile is rejected (no silent "mock").
        let bad = serde_json::from_str::<AuraConfig>(
            r#"{"connections":{"profiles":[{"kind":"codex","transport":"telepathy"}]}}"#,
        );
        assert!(bad.is_err(), "unknown transport must be rejected");
    }
}
