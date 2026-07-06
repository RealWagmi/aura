//! Host resolution — pick the [`HostAdapter`] for the environment the binary
//! was launched in.
//!
//! Triggers are deliberately NOT unified across hosts: a single
//! universal trigger would break at least two hosts. So the registry resolves
//! by each host's *native* launch signal rather than by probing which AI tools
//! happen to be installed — `detect()` ("is this host present on the machine")
//! is a separate, weaker question that mis-fires on a dev box with several
//! hosts installed at once (e.g. both `~/.codex` and `~/.claude` exist). The
//! launch signal, by contrast, says which host actually invoked us:
//!
//! - **`AURA_HOST`** — an explicit operator/launcher override (`claude` /
//!   `codex` / `hermes` / `openclaw`). Wins over everything; this is how a
//!   per-host shim (e.g. the Hermes `codexini-call` skill, which has no ambient
//!   env of its own) names itself.
//! - **`AURA_AGENT=codex`** — Codex's documented launcher trigger.
//! - **OpenClaw identity env** — set when the binary runs inside an OpenClaw
//!   session: the `OPENCLAW_MCP_SESSION_KEY` + `OPENCLAW_MCP_ACCOUNT_ID` pair
//!   (CLI-backend agent runs), or `OPENCLAW_SHELL=exec` (exec-tool spawns);
//!   the legacy `OPENCLAW_SESSION_KEY` + `OPENCLAW_ACCOUNT_ID` pair is still
//!   honoured for custom shims.
//! - Otherwise **Claude**, the ambient default and the executing dispatcher.
//!
//! Reading context is always fail-open, so even a force-selected host with a
//! thin/empty store still composes a usable [`Brief`](aura_core::brief::Brief)
//! and dials — resolution never blocks a call.

use std::path::PathBuf;
use std::sync::Arc;

use aura_core::host::HostKind;

use crate::{ClaudeAdapter, CodexAdapter, HermesAdapter, HostAdapter, OpenClawAdapter};

/// Operator/launcher override: force a specific host regardless of ambient
/// signals. Case-insensitive; `openclaw` and `open_claw` both map to OpenClaw.
const HOST_OVERRIDE_VAR: &str = "AURA_HOST";
/// Codex's documented launcher trigger variable: `AURA_AGENT=codex`.
const CODEX_AGENT_VAR: &str = "AURA_AGENT";
/// The value of [`CODEX_AGENT_VAR`] that selects Codex.
const CODEX_AGENT_VALUE: &str = "codex";
/// OpenClaw session identity for CLI-backend agent runs (source-verified:
/// `src/agents/cli-runner/prepare.ts` sets the `OPENCLAW_MCP_*` family).
const OPENCLAW_MCP_SESSION_VAR: &str = "OPENCLAW_MCP_SESSION_KEY";
/// OpenClaw account identity — the other half of the CLI-backend signal.
const OPENCLAW_MCP_ACCOUNT_VAR: &str = "OPENCLAW_MCP_ACCOUNT_ID";
/// OpenClaw exec-tool spawns carry `OPENCLAW_SHELL=exec`
/// (`src/agents/bash-tools.exec.ts`) — the signal when aura is launched via the
/// exec tool rather than a CLI backend.
const OPENCLAW_SHELL_VAR: &str = "OPENCLAW_SHELL";
/// Legacy/custom-shim identity pair (kept for integrations that export them;
/// these names do NOT exist in upstream OpenClaw).
const OPENCLAW_LEGACY_SESSION_VAR: &str = "OPENCLAW_SESSION_KEY";
const OPENCLAW_LEGACY_ACCOUNT_VAR: &str = "OPENCLAW_ACCOUNT_ID";
/// Optional pin for the in-call dispatch model (`dispatch_model`): Claude →
/// `claude -p --model`, Codex → app-server `params.model`. When UNSET, Claude
/// auto-matches the live chat session's model (from the transcript); Codex does
/// NOT auto-match (it runs on the app-server default), so this env is the only
/// way to control the Codex dispatch model. The server is env-driven (no config
/// file is loaded), so this env is how the otherwise-dormant
/// `ClaudeConfig::dispatch_model` field is populated at runtime.
const DISPATCH_MODEL_VAR: &str = "AURA_DISPATCH_MODEL";
/// Default `codex` app-server binary (mirrors `CodexAdapter::new`), so a
/// dispatch-model pin can be applied without changing the binary.
const CODEX_DEFAULT_BIN: &str = "codex";

/// Parse an `AURA_HOST` override value into a [`HostKind`]. Returns `None` for
/// an unset/blank/unrecognized value (the caller then falls through to the
/// native signals and finally the Claude default).
fn parse_override(raw: Option<&str>) -> Option<HostKind> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(HostKind::Claude),
        "codex" => Some(HostKind::Codex),
        "hermes" => Some(HostKind::Hermes),
        "openclaw" | "open_claw" => Some(HostKind::OpenClaw),
        _ => None,
    }
}

/// The pure resolution rule, factored out of [`resolve_host`] so it can be
/// tested without mutating process-global environment variables (which would
/// race across parallel test threads). Order: explicit override → native
/// launch signal → Claude default.
fn resolve_kind(
    override_raw: Option<&str>,
    codex_agent: Option<&str>,
    openclaw_identity: bool,
) -> HostKind {
    if let Some(kind) = parse_override(override_raw) {
        return kind;
    }
    if codex_agent == Some(CODEX_AGENT_VALUE) {
        return HostKind::Codex;
    }
    if openclaw_identity {
        return HostKind::OpenClaw;
    }
    HostKind::Claude
}

/// Build an execution-ready [`HostAdapter`] for `kind`, rooted at `cwd`.
///
/// Claude is constructed in *executing* mode so in-call `start_agent_task`
/// dispatches run `claude -p` in the repo; the other hosts
/// execute through their own native mechanism, so their default constructor is
/// already dispatch-ready. `HostKind::Other` falls back to Claude.
pub fn build_host(kind: HostKind, cwd: impl Into<PathBuf>) -> Arc<dyn HostAdapter> {
    let cwd = cwd.into();
    // Optional dispatch-model pin (`AURA_DISPATCH_MODEL`). Empty/whitespace is
    // treated as unset. Meaningful only for the hosts with a per-call model knob
    // (Claude, Codex); OpenClaw runs the consult in its live session (its own
    // model) and Hermes bakes the model into its worker command, so both ignore it.
    let dispatch_model = std::env::var(DISPATCH_MODEL_VAR)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    match kind {
        HostKind::Claude | HostKind::Other => Arc::new(
            ClaudeAdapter::executing_with_dispatch_model(cwd, dispatch_model),
        ),
        HostKind::Codex => Arc::new(match dispatch_model {
            Some(model) => CodexAdapter::with_binary_and_model(cwd, CODEX_DEFAULT_BIN, Some(model)),
            None => CodexAdapter::new(cwd),
        }),
        HostKind::Hermes => Arc::new(HermesAdapter::new(cwd)),
        HostKind::OpenClaw => Arc::new(OpenClawAdapter::new(cwd)),
    }
}

/// Resolve the host adapter for the current launch, reading the
/// native launch signals from the environment. See the module docs for the
/// resolution order. Always returns an adapter — Claude is the default — so a
/// binary can unconditionally proceed to compose a brief and dial.
pub fn resolve_host(cwd: impl Into<PathBuf>) -> Arc<dyn HostAdapter> {
    let override_raw = std::env::var(HOST_OVERRIDE_VAR).ok();
    let codex_agent = std::env::var(CODEX_AGENT_VAR).ok();
    // OpenClaw ambient signals, strongest first: the CLI-backend identity pair
    // (`OPENCLAW_MCP_*`), an exec-tool spawn (`OPENCLAW_SHELL=exec`), or the
    // legacy custom-shim pair.
    let openclaw_identity = (std::env::var_os(OPENCLAW_MCP_SESSION_VAR).is_some()
        && std::env::var_os(OPENCLAW_MCP_ACCOUNT_VAR).is_some())
        || std::env::var(OPENCLAW_SHELL_VAR)
            .map(|v| v.trim().eq_ignore_ascii_case("exec"))
            .unwrap_or(false)
        || (std::env::var_os(OPENCLAW_LEGACY_SESSION_VAR).is_some()
            && std::env::var_os(OPENCLAW_LEGACY_ACCOUNT_VAR).is_some());
    let kind = resolve_kind(
        override_raw.as_deref(),
        codex_agent.as_deref(),
        openclaw_identity,
    );
    build_host(kind, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_parses_each_host_case_insensitively() {
        assert_eq!(parse_override(Some("claude")), Some(HostKind::Claude));
        assert_eq!(parse_override(Some("Codex")), Some(HostKind::Codex));
        assert_eq!(parse_override(Some("  HERMES ")), Some(HostKind::Hermes));
        assert_eq!(parse_override(Some("openclaw")), Some(HostKind::OpenClaw));
        assert_eq!(parse_override(Some("open_claw")), Some(HostKind::OpenClaw));
        assert_eq!(parse_override(Some("nonsense")), None);
        assert_eq!(parse_override(Some("")), None);
        assert_eq!(parse_override(None), None);
    }

    #[test]
    fn explicit_override_wins_over_native_signals() {
        // Even with the Codex launcher env AND OpenClaw identity present, an
        // explicit override forces the named host.
        assert_eq!(
            resolve_kind(Some("hermes"), Some("codex"), true),
            HostKind::Hermes
        );
    }

    #[test]
    fn codex_launcher_env_selects_codex() {
        assert_eq!(resolve_kind(None, Some("codex"), false), HostKind::Codex);
        // A different AURA_AGENT value does not select Codex.
        assert_eq!(resolve_kind(None, Some("other"), false), HostKind::Claude);
    }

    #[test]
    fn openclaw_identity_selects_openclaw_when_no_stronger_signal() {
        assert_eq!(resolve_kind(None, None, true), HostKind::OpenClaw);
        // Codex's launcher env is the stronger signal and takes precedence.
        assert_eq!(resolve_kind(None, Some("codex"), true), HostKind::Codex);
    }

    #[test]
    fn defaults_to_claude_with_no_signals() {
        assert_eq!(resolve_kind(None, None, false), HostKind::Claude);
    }

    #[test]
    fn build_host_returns_matching_kind() {
        let cwd = std::path::Path::new("/tmp/aura-registry-test");
        assert_eq!(build_host(HostKind::Claude, cwd).kind(), HostKind::Claude);
        assert_eq!(build_host(HostKind::Codex, cwd).kind(), HostKind::Codex);
        assert_eq!(build_host(HostKind::Hermes, cwd).kind(), HostKind::Hermes);
        assert_eq!(
            build_host(HostKind::OpenClaw, cwd).kind(),
            HostKind::OpenClaw
        );
        // Other falls back to the Claude adapter.
        assert_eq!(build_host(HostKind::Other, cwd).kind(), HostKind::Claude);
    }
}
