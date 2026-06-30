//! Instruction composer: pack a persona prefix + priority-ranked context
//! chunks into one `instructions` string that fits a token budget.
//!
//! Drop order on overflow:
//! `Ambient → Message → Fact → AnticipatedQuestion → Persona`. `Persona` is
//! **never** dropped (the caller cannot ship a model that doesn't know who it
//! is). `anchor_survival` = |Critical+High kept| / |Critical+High in| is the
//! regression metric.

use aura_core::brief::Brief;

/// Priority levels for a section. Higher discriminant = more important.
/// `Critical` + `High` form the **anchor set** that `anchor_survival` tracks.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub enum Priority {
    Critical = 100,
    High = 80,
    Medium = 60,
    Low = 40,
    Ambient = 20,
}

/// What a single chunk of pre-redacted context represents. Drop order is
/// `Ambient → Message → Fact → AnticipatedQuestion → Persona`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Section {
    Persona,
    AnticipatedQuestion,
    Fact { id: String },
    Message { ts: u64 },
    Ambient { source: AmbientSource },
}

/// Where an `Ambient` section came from — telemetry, and so the feeder can
/// drop one ambient class without affecting another.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AmbientSource {
    HermesActivity,
    FileWatch,
    ChatTail,
}

/// One pre-redacted chunk of context, ready to be packed. The composer never
/// re-redacts (text is already redacted by the producer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionChunk {
    pub section: Section,
    pub priority: Priority,
    pub text: String,
    pub estimated_tokens: u32,
}

/// Result of [`compose_instructions_by_priority`].
#[derive(Clone, Debug, PartialEq)]
pub struct ComposeOutput {
    pub instructions: String,
    pub used_tokens: u32,
    pub dropped: Vec<Section>,
    /// Fraction of `Critical + High` sections that survived budgeting, in
    /// `[0.0, 1.0]`; `1.0` when there are zero anchor sections in input.
    pub anchor_survival: f32,
}

/// Section-type drop order. Lower index = dropped first.
fn section_drop_rank(section: &Section) -> u8 {
    match section {
        Section::Ambient { .. } => 0,
        Section::Message { .. } => 1,
        Section::Fact { .. } => 2,
        Section::AnticipatedQuestion => 3,
        Section::Persona => 4,
    }
}

/// Rough token estimate: ~4 chars/token.
pub fn estimate_tokens_from_chars(chars: usize) -> u32 {
    ((chars as u64).div_ceil(4)).min(u32::MAX as u64) as u32
}

/// Pack `persona` + ranked `sections` into a single `instructions` string that
/// fits within `budget_tokens`. See the module docs for the drop policy.
pub fn compose_instructions_by_priority(
    persona: &str,
    sections: Vec<SectionChunk>,
    budget_tokens: u32,
) -> ComposeOutput {
    let persona_tokens = estimate_tokens_from_chars(persona.len());
    let total_anchor_in = sections
        .iter()
        .filter(|s| matches!(s.priority, Priority::Critical | Priority::High))
        .count();

    let n = sections.len();
    let mut keep = vec![true; n];

    let chunk_tokens_sum = |keep: &[bool], sections: &[SectionChunk]| -> u32 {
        sections
            .iter()
            .zip(keep.iter())
            .filter_map(|(s, &k)| if k { Some(s.estimated_tokens) } else { None })
            .sum()
    };

    // Drop order: (section-type rank, priority asc, size desc, insertion order).
    let mut drop_order: Vec<usize> = (0..n)
        .filter(|&i| !matches!(sections[i].section, Section::Persona))
        .collect();
    drop_order.sort_by(|&a, &b| {
        let sa = &sections[a];
        let sb = &sections[b];
        section_drop_rank(&sa.section)
            .cmp(&section_drop_rank(&sb.section))
            .then(sa.priority.cmp(&sb.priority))
            .then(sb.estimated_tokens.cmp(&sa.estimated_tokens))
            .then(a.cmp(&b))
    });

    let mut idx_cursor = 0;
    while persona_tokens + chunk_tokens_sum(&keep, &sections) > budget_tokens
        && idx_cursor < drop_order.len()
    {
        let i = drop_order[idx_cursor];
        keep[i] = false;
        idx_cursor += 1;
    }

    let mut parts: Vec<&str> = Vec::with_capacity(n + 1);
    if !persona.is_empty() {
        parts.push(persona);
    }
    let mut dropped: Vec<Section> = Vec::new();
    for (i, chunk) in sections.iter().enumerate() {
        if keep[i] {
            parts.push(&chunk.text);
        } else {
            dropped.push(chunk.section.clone());
        }
    }
    let instructions = parts.join("\n\n");
    let used_tokens = persona_tokens + chunk_tokens_sum(&keep, &sections);

    let anchors_kept = sections
        .iter()
        .zip(keep.iter())
        .filter(|(s, &k)| k && matches!(s.priority, Priority::Critical | Priority::High))
        .count();
    let anchor_survival = if total_anchor_in == 0 {
        1.0
    } else {
        anchors_kept as f32 / total_anchor_in as f32
    };

    if used_tokens > budget_tokens {
        // Pathological: persona alone exceeds budget. Persona is kept intact
        // and we log so the caller can surface it.
        eprintln!(
            "aura-voice: compose_instructions_by_priority returned {used_tokens} tokens > budget {budget_tokens} (persona alone over budget)"
        );
    }

    ComposeOutput {
        instructions,
        used_tokens,
        dropped,
        anchor_survival,
    }
}

/// Build a `SectionChunk` with an estimated token count from its text.
fn brief_chunk(section: Section, priority: Priority, text: String) -> SectionChunk {
    let estimated_tokens = estimate_tokens_from_chars(text.len());
    SectionChunk {
        section,
        priority,
        text,
        estimated_tokens,
    }
}

/// Turn a [`Brief`] into ranked [`SectionChunk`]s — the single packer shared by
/// LOCAL and REMOTE. `current_focus` becomes a high-priority Fact;
/// each recent message becomes a Message chunk (older → lower `ts`). The
/// persona is supplied separately to [`compose_instructions_by_priority`] and
/// is never dropped.
pub fn sections_from_brief(brief: &Brief) -> Vec<SectionChunk> {
    let mut out = Vec::new();
    if !brief.context.current_focus.is_empty() {
        out.push(brief_chunk(
            Section::Fact {
                id: "current_focus".to_owned(),
            },
            Priority::High,
            format!("Current focus: {}", brief.context.current_focus),
        ));
    }
    for (i, m) in brief.context.recent_messages_verbatim.iter().enumerate() {
        let who = if m.role == "assistant" || m.role == "hermes" {
            "assistant"
        } else {
            "user"
        };
        out.push(brief_chunk(
            Section::Message { ts: i as u64 },
            Priority::Medium,
            format!("[{who}] {}", m.text),
        ));
    }
    out
}

/// Compose the final `instructions` string for a session from a persona and a
/// [`Brief`], fitting `budget_tokens`. Fail-open: a thin/empty brief yields
/// just the persona (the call still proceeds).
pub fn instructions_from_brief(persona: &str, brief: &Brief, budget_tokens: u32) -> String {
    compose_instructions_by_priority(persona, sections_from_brief(brief), budget_tokens)
        .instructions
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::brief::RecentMessage;

    fn chunk(section: Section, priority: Priority, text: &str) -> SectionChunk {
        SectionChunk {
            section,
            priority,
            estimated_tokens: estimate_tokens_from_chars(text.len()),
            text: text.to_owned(),
        }
    }

    #[test]
    fn persona_is_never_dropped_even_over_budget() {
        let persona = "You are Aura. ".repeat(50); // big persona
        let out = compose_instructions_by_priority(
            &persona,
            vec![chunk(
                Section::Ambient {
                    source: AmbientSource::ChatTail,
                },
                Priority::Ambient,
                "ambient noise",
            )],
            1, // absurdly small budget
        );
        assert!(out.instructions.contains("You are Aura."));
        assert!(out.dropped.contains(&Section::Ambient {
            source: AmbientSource::ChatTail
        }));
    }

    #[test]
    fn anchor_survival_is_one_when_no_anchors() {
        let out = compose_instructions_by_priority("persona", vec![], 1000);
        assert_eq!(out.anchor_survival, 1.0);
        assert_eq!(out.instructions, "persona");
    }

    #[test]
    fn drops_ambient_before_anchors_under_pressure() {
        let big = "x".repeat(4000); // ~1000 tokens
        let sections = vec![
            chunk(
                Section::Ambient {
                    source: AmbientSource::FileWatch,
                },
                Priority::Ambient,
                &big,
            ),
            chunk(
                Section::Fact { id: "f1".into() },
                Priority::Critical,
                "key fact",
            ),
        ];
        let out = compose_instructions_by_priority("persona", sections, 60);
        // Ambient dropped, the Critical fact survives → full anchor survival.
        assert!(out.instructions.contains("key fact"));
        assert!(out
            .dropped
            .iter()
            .any(|s| matches!(s, Section::Ambient { .. })));
        assert_eq!(out.anchor_survival, 1.0);
    }

    #[test]
    fn keeps_everything_within_budget() {
        let sections = vec![
            chunk(Section::Message { ts: 1 }, Priority::Medium, "hello there"),
            chunk(Section::Fact { id: "f".into() }, Priority::High, "a fact"),
        ];
        let out = compose_instructions_by_priority("persona", sections, 10_000);
        assert!(out.dropped.is_empty());
        assert!(out.instructions.contains("hello there"));
        assert!(out.instructions.contains("a fact"));
    }

    #[test]
    fn instructions_from_brief_weaves_persona_and_context() {
        let mut brief = Brief::default();
        brief.context.current_focus = "refactor auth".into();
        brief.context.recent_messages_verbatim = vec![
            RecentMessage {
                role: "user".into(),
                text: "make it faster".into(),
                channel: "claude-code".into(),
                ts_iso: None,
            },
            RecentMessage {
                role: "assistant".into(),
                text: "on it".into(),
                channel: "claude-code".into(),
                ts_iso: None,
            },
        ];
        let out = instructions_from_brief("You are Aura.", &brief, 10_000);
        assert!(out.starts_with("You are Aura."));
        assert!(out.contains("Current focus: refactor auth"));
        assert!(out.contains("[user] make it faster"));
        assert!(out.contains("[assistant] on it"));
    }

    #[test]
    fn instructions_from_thin_brief_is_just_persona() {
        // Fail-open: an empty brief still composes (persona only).
        let out = instructions_from_brief("You are Aura.", &Brief::default(), 10_000);
        assert_eq!(out, "You are Aura.");
    }
}
