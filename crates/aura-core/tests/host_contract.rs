use aura_core::{
    HostMemoryCard, HostMemoryPriority, HostMemorySection, HostMemorySource, HostSessionIdentity,
    HostToolDescriptor, ToolManifest,
};

#[test]
fn host_card_serializes_openclaw_session_keys_without_host_prefixed_fields() {
    let mut identity = HostSessionIdentity::openclaw("principal-1", "main", "session-main-123");
    identity.requester_session_key = Some("requester-session-456".to_owned());

    let mut card = HostMemoryCard::new(identity, 1_715_000_000_000);
    card.memory.push(HostMemorySection::untrusted(
        "memory",
        "Durable memory",
        HostMemorySource::LongTermMemory,
        HostMemoryPriority::High,
        "User prefers short direct answers.",
    ));
    card.tools = ToolManifest::new(vec![HostToolDescriptor::read_only(
        "memory_search",
        "Search host memory",
    )]);

    let json = serde_json::to_string(&card).expect("host card serializes");

    assert!(json.contains(r#""host":"open_claw""#));
    assert!(json.contains(r#""agent_id":"main""#));
    assert!(json.contains(r#""session_key":"session-main-123""#));
    assert!(json.contains(r#""requester_session_key":"requester-session-456""#));
    assert!(!json.contains("openclaw_"));
    assert!(!json.contains("open_claw_"));
    assert!(!json.contains("hermes_"));
}

#[test]
fn host_privacy_defaults_keep_voice_from_disabling_filter() {
    let identity = HostSessionIdentity::openclaw("principal-1", "main", "session-main-123");
    let card = HostMemoryCard::new(identity, 1_715_000_000_000);

    assert!(card.privacy.redact_secrets);
    assert!(card.privacy.privacy_filter_enabled);
    assert!(!card.privacy.voice_can_disable_privacy_filter);
}

#[test]
fn host_memory_sections_are_untrusted_and_redacted_by_default() {
    let section = HostMemorySection::untrusted(
        "daily",
        "Daily notes",
        HostMemorySource::DailyNote,
        HostMemoryPriority::Medium,
        "Synthetic note only.",
    );

    assert!(section.untrusted);
    assert!(section.redacted);
}

#[test]
fn host_tool_manifest_marks_destructive_actions_as_confirmed() {
    let tool = HostToolDescriptor::confirmed_action("file_delete", "Delete a file");

    assert!(!tool.read_only);
    assert!(tool.destructive);
    assert!(tool.requires_confirmation);
}
