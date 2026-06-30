//! The one permitted OpenClaw dispatch frame + its security gate.
//!
//! Voice can ask the bound OpenClaw session to
//! consult — and nothing else. The single allowed frame is the
//! `openclaw_agent_consult` tool on the `talk.client.toolCall` gateway method.
//!
//! ## Security contract
//!
//! Voice must NEVER pass a direct tool name, a confirmation/approval flag, or
//! an arbitrary session key. [`reject_direct_overrides`] hard-fails on any of
//! the 18 [`FORBIDDEN_DIRECT_FIELDS`] keys with
//! [`DispatchError::DirectToolOverrideNotAllowed`], and every identity field is
//! checked for embedded line breaks ([`assert_no_line_breaks`]). The account
//! must match when both the identity and the request name one.

use serde_json::{json, Map, Value};

use aura_core::host::HostSessionIdentity;

/// The gateway RPC method the consult dispatch travels on.
pub const OPENCLAW_DISPATCH_METHOD: &str = "talk.client.toolCall";
/// The single tool voice is allowed to invoke through the gateway.
pub const OPENCLAW_AGENT_CONSULT_TOOL: &str = "openclaw_agent_consult";

/// The 18 forbidden direct-override keys. If any appears in the dispatch input
/// object, the request is a bypass attempt and is rejected. Case-sensitive,
/// exact match (mirrors the JS `Set`). Ordered to match `dispatch.js`.
pub const FORBIDDEN_DIRECT_FIELDS: &[&str] = &[
    "agentId",
    "agent_id",
    "approval",
    "approvalId",
    "approved",
    "confirm",
    "confirmed",
    "destructive",
    "directTool",
    "direct_tool",
    "sessionKey",
    "session_key",
    "targetSessionKey",
    "target_session_key",
    "tool",
    "toolName",
    "toolPayload",
    "tool_payload",
];

/// Errors raised while validating/building a consult dispatch. The `code()`
/// strings match the JS `OpenClawDispatchError` codes verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DispatchError {
    /// A [`FORBIDDEN_DIRECT_FIELDS`] key appeared in the input.
    #[error("direct_tool_override_not_allowed: field \"{0}\" is not allowed")]
    DirectToolOverrideNotAllowed(String),
    /// A required identity/string field was missing or blank.
    #[error("missing_dispatch_field: {0} is required")]
    MissingField(String),
    /// An identity/string field contained a CR or LF.
    #[error("invalid_dispatch_field: {0} must not contain line breaks")]
    LineBreak(String),
    /// The dispatch identity does not belong to the active account.
    #[error("account_mismatch: dispatch identity does not belong to the active account")]
    AccountMismatch,
}

/// The return identity echoed back so the async callback can be routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnIdentity {
    pub account_id: String,
    pub agent_id: String,
    pub channel: String,
    pub reply_target: String,
}

/// A fully built, validated consult dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultDispatch {
    /// Always [`OPENCLAW_DISPATCH_METHOD`].
    pub method: String,
    /// The gateway `params` object: `{ sessionKey, callId, name, args }`.
    pub params: Value,
    /// The return identity for the callback leg.
    pub return_identity: ReturnIdentity,
}

/// Optional extra fields the consult question may carry (never identity).
#[derive(Debug, Clone, Default)]
pub struct ConsultExtras {
    /// Optional JSON-ish context string forwarded to the OpenClaw agent.
    pub context: Option<String>,
    /// Optional response-format hint.
    pub response_style: Option<String>,
}

/// Reject any forbidden direct-override key present in `input`. Run FIRST,
/// before any field is read (mirrors `rejectDirectOverrides`).
pub fn reject_direct_overrides(input: &Map<String, Value>) -> Result<(), DispatchError> {
    for key in input.keys() {
        if FORBIDDEN_DIRECT_FIELDS.contains(&key.as_str()) {
            return Err(DispatchError::DirectToolOverrideNotAllowed(key.clone()));
        }
    }
    Ok(())
}

/// Reject CR/LF in an identity field (mirrors `assertNoLineBreaks`).
fn assert_no_line_breaks(value: &str, label: &str) -> Result<(), DispatchError> {
    if value.contains('\r') || value.contains('\n') {
        return Err(DispatchError::LineBreak(label.to_owned()));
    }
    Ok(())
}

/// Trim and require a non-empty string (mirrors `requiredString`).
fn required_string(value: Option<&str>, label: &str) -> Result<String, DispatchError> {
    match value.map(str::trim) {
        Some(v) if !v.is_empty() => Ok(v.to_owned()),
        _ => Err(DispatchError::MissingField(label.to_owned())),
    }
}

/// Build the gateway params for a consult dispatch from the bound identity and
/// the voice-approved question. This is the PURE builder; sending is the WS
/// client's job. Runs the security gate first.
///
/// `extra_input_keys` carries any additional keys the caller would forward
/// verbatim (e.g. from a structured request); they are passed through the
/// forbidden-field gate so a bypass attempt is rejected even when the question
/// is constructed indirectly. For the voice path this is normally empty.
pub fn build_openclaw_consult_dispatch(
    identity: &HostSessionIdentity,
    call_id: &str,
    question: &str,
    extras: &ConsultExtras,
    extra_input_keys: &Map<String, Value>,
) -> Result<ConsultDispatch, DispatchError> {
    // Step 1 — forbidden-field gate runs FIRST, on the raw input keys.
    reject_direct_overrides(extra_input_keys)?;

    // Step 2 — pull and validate the required identity + request fields.
    let session_key = required_string(
        identity.session_key.as_deref(),
        "sessionIdentity.session_key",
    )?;
    let account_id = required_string(identity.account_id.as_deref(), "sessionIdentity.account_id")?;
    let agent_id = required_string(identity.agent_id.as_deref(), "sessionIdentity.agent_id")?;
    let channel = required_string(identity.channel.as_deref(), "sessionIdentity.channel")?;
    let reply_target =
        required_string(identity.reply_to.as_deref(), "sessionIdentity.reply_target")?;
    let call_id = required_string(Some(call_id), "callId")?;
    let question = required_string(Some(question), "question")?;

    assert_no_line_breaks(&session_key, "sessionIdentity.session_key")?;
    assert_no_line_breaks(&account_id, "sessionIdentity.account_id")?;
    assert_no_line_breaks(&agent_id, "sessionIdentity.agent_id")?;
    assert_no_line_breaks(&channel, "sessionIdentity.channel")?;
    assert_no_line_breaks(&reply_target, "sessionIdentity.reply_target")?;
    assert_no_line_breaks(&call_id, "callId")?;

    // Account-match guard: when the caller named an account, it must equal the
    // identity's account (mirrors `assertAccountMatches`).
    if let Some(expected) = extra_input_keys
        .get("accountId")
        .or_else(|| extra_input_keys.get("account_id"))
        .or_else(|| extra_input_keys.get("expectedAccountId"))
        .or_else(|| extra_input_keys.get("expected_account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if expected != account_id {
            return Err(DispatchError::AccountMismatch);
        }
    }

    // Step 3 — assemble args (question + optional context / responseStyle).
    let mut args = Map::new();
    args.insert("question".to_owned(), Value::String(question));
    if let Some(ctx) = extras
        .context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.insert("context".to_owned(), Value::String(ctx.to_owned()));
    }
    if let Some(style) = extras
        .response_style
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.insert("responseStyle".to_owned(), Value::String(style.to_owned()));
    }

    Ok(ConsultDispatch {
        method: OPENCLAW_DISPATCH_METHOD.to_owned(),
        params: json!({
            "sessionKey": session_key,
            "callId": call_id,
            "name": OPENCLAW_AGENT_CONSULT_TOOL,
            "args": Value::Object(args),
        }),
        return_identity: ReturnIdentity {
            account_id,
            agent_id,
            channel,
            reply_target,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_identity() -> HostSessionIdentity {
        let mut id = HostSessionIdentity::openclaw("principal-1", "agent-7", "sess-key-xyz");
        id.account_id = Some("acct-123".to_owned());
        id.channel = Some("telegram".to_owned());
        id.reply_to = Some("chat-9001".to_owned());
        id
    }

    #[test]
    fn builds_consult_with_expected_shape() {
        let dispatch = build_openclaw_consult_dispatch(
            &full_identity(),
            "call-abc",
            "Refactor the planner module",
            &ConsultExtras::default(),
            &Map::new(),
        )
        .unwrap();

        assert_eq!(dispatch.method, "talk.client.toolCall");
        assert_eq!(dispatch.params["sessionKey"], "sess-key-xyz");
        assert_eq!(dispatch.params["callId"], "call-abc");
        assert_eq!(dispatch.params["name"], "openclaw_agent_consult");
        assert_eq!(
            dispatch.params["args"]["question"],
            "Refactor the planner module"
        );
        // No optional fields when extras are empty.
        assert!(dispatch.params["args"].get("context").is_none());
        assert!(dispatch.params["args"].get("responseStyle").is_none());

        let ret = &dispatch.return_identity;
        assert_eq!(ret.account_id, "acct-123");
        assert_eq!(ret.agent_id, "agent-7");
        assert_eq!(ret.channel, "telegram");
        assert_eq!(ret.reply_target, "chat-9001");
    }

    #[test]
    fn includes_optional_context_and_style() {
        let extras = ConsultExtras {
            context: Some("{\"repo\":\"aura\"}".to_owned()),
            response_style: Some("concise".to_owned()),
        };
        let dispatch = build_openclaw_consult_dispatch(
            &full_identity(),
            "call-abc",
            "go",
            &extras,
            &Map::new(),
        )
        .unwrap();
        assert_eq!(dispatch.params["args"]["context"], "{\"repo\":\"aura\"}");
        assert_eq!(dispatch.params["args"]["responseStyle"], "concise");
    }

    #[test]
    fn every_forbidden_field_is_rejected() {
        for forbidden in FORBIDDEN_DIRECT_FIELDS {
            let mut input = Map::new();
            input.insert((*forbidden).to_owned(), json!("anything"));
            let err = build_openclaw_consult_dispatch(
                &full_identity(),
                "call-abc",
                "go",
                &ConsultExtras::default(),
                &input,
            )
            .unwrap_err();
            match err {
                DispatchError::DirectToolOverrideNotAllowed(key) => {
                    assert_eq!(&key, *forbidden);
                }
                other => panic!("expected override rejection for {forbidden}, got {other:?}"),
            }
        }
        assert_eq!(FORBIDDEN_DIRECT_FIELDS.len(), 18);
    }

    #[test]
    fn security_gate_runs_before_field_reads() {
        // A forbidden key must be rejected even when identity is incomplete,
        // proving the gate runs first.
        let mut blank = HostSessionIdentity::openclaw("p", "a", "s");
        blank.account_id = None;
        blank.channel = None;
        blank.reply_to = None;
        let mut input = Map::new();
        input.insert("tool".to_owned(), json!("rm -rf"));
        let err =
            build_openclaw_consult_dispatch(&blank, "call", "q", &ConsultExtras::default(), &input)
                .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::DirectToolOverrideNotAllowed(_)
        ));
    }

    #[test]
    fn clean_input_passes_the_gate() {
        let mut input = Map::new();
        input.insert("note".to_owned(), json!("not forbidden"));
        assert!(reject_direct_overrides(&input).is_ok());
    }

    #[test]
    fn missing_identity_field_is_reported() {
        let mut id = full_identity();
        id.channel = None;
        let err = build_openclaw_consult_dispatch(
            &id,
            "call",
            "q",
            &ConsultExtras::default(),
            &Map::new(),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::MissingField(f) if f.contains("channel")));
    }

    #[test]
    fn line_breaks_in_identity_are_rejected() {
        let mut id = full_identity();
        id.session_key = Some("sess\nkey".to_owned());
        let err = build_openclaw_consult_dispatch(
            &id,
            "call",
            "q",
            &ConsultExtras::default(),
            &Map::new(),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::LineBreak(_)));
    }

    #[test]
    fn account_mismatch_is_rejected() {
        let mut input = Map::new();
        input.insert("accountId".to_owned(), json!("acct-OTHER"));
        let err = build_openclaw_consult_dispatch(
            &full_identity(),
            "call",
            "q",
            &ConsultExtras::default(),
            &input,
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::AccountMismatch));
    }

    #[test]
    fn matching_account_passes() {
        let mut input = Map::new();
        input.insert("account_id".to_owned(), json!("acct-123"));
        assert!(build_openclaw_consult_dispatch(
            &full_identity(),
            "call",
            "q",
            &ConsultExtras::default(),
            &input,
        )
        .is_ok());
    }
}
