use aura_core::{
    load_session, redact_secrets, save_session_atomic, session_path, speech_safe_summary,
    AuraConfig, CallbackMode, CheckpointEvent, CheckpointKind, CheckpointStore, HistoryEvent,
    Session, TaskEnvelope, TaskHandoffState, TaskResult,
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn fuzz_redaction_speech_config_history_and_state_surfaces() {
    let mut seed = 0xA0DA_2026_u64;
    for _ in 0..256 {
        let input = fuzz_string(&mut seed);
        let with_secret =
            format!("{input} API_KEY=abc12345678901234567890 src/auth.rs:217 ```let x = 1;```");

        let redacted = redact_secrets(&with_secret);
        assert!(!redacted.contains("abc12345678901234567890"));

        let spoken = speech_safe_summary(&with_secret);
        assert!(!spoken.contains("abc12345678901234567890"));
        // Path with file extension must be stripped (replaced with
        // "a project file" by path_regex).
        assert!(!spoken.contains("src/auth.rs"));
        // A standalone "217" from fuzz noise can leak through the canned-phrase
        // substitution, and that's fine (Aura saying "I added 217 tests" is
        // OK). The actual concern is the line-number reference pattern from the
        // path:line construct ('src/auth.rs:217') and the bare-line form
        // ('line 217'); both should be stripped by path_regex and
        // line_number_regex respectively.
        assert!(!spoken.contains("src/auth.rs:217"));
        assert!(!spoken.contains(":217"));
        assert!(!spoken.contains("```"));

        let _ = serde_json::from_str::<AuraConfig>(&input);
        let event = HistoryEvent::new("fuzz", &with_secret);
        assert!(!event.speech.contains("abc12345678901234567890"));

        let envelope = TaskEnvelope::new(
            &with_secret,
            vec![with_secret.clone()],
            "Aura",
            CallbackMode::PingFirst,
            "local-test-approval",
        );
        assert!(!envelope.user_intent.contains("abc12345678901234567890"));
        assert!(!envelope.constraints[0].contains("abc12345678901234567890"));
    }
}

#[test]
fn fuzz_checkpoint_event_speech_is_always_redacted() {
    // Hammer CheckpointEvent::new across 256 seeded raw payloads — each one
    // includes a known secret + a known path:line + a fenced code block, so
    // the speech-safe filter has to strip all three regardless of what
    // junk surrounds them.
    let mut seed = 0xC4EC_2026_u64;
    for i in 0..256 {
        let prefix = fuzz_string(&mut seed);
        let suffix = fuzz_string(&mut seed);
        let raw = format!(
            "{prefix} API_KEY=abc12345678901234567890 src/auth.rs:217 ```secret={i}``` {suffix}"
        );

        let kind = pick_kind(next_u64(&mut seed));
        let event = CheckpointEvent::new(kind, raw.clone());

        // Construction-time redaction: nothing the speech filter is
        // supposed to remove may survive.
        assert!(
            !event.speech.contains("abc12345678901234567890"),
            "secret leaked into checkpoint speech for raw={raw:?}"
        );
        assert!(
            !event.speech.contains("src/auth.rs"),
            "path leaked into checkpoint speech for raw={raw:?}"
        );
        // A standalone "217" from fuzz noise can survive the canned-phrase
        // substitution — that's OK. Assert the actual line-number reference
        // patterns instead.
        assert!(
            !event.speech.contains("src/auth.rs:217"),
            "path:line construct leaked into checkpoint speech for raw={raw:?}"
        );
        assert!(
            !event.speech.contains(":217"),
            "':217' line-number suffix leaked into checkpoint speech for raw={raw:?}"
        );
        assert!(
            !event.speech.contains("```"),
            "code fence leaked into checkpoint speech for raw={raw:?}"
        );
        assert_eq!(event.kind, kind);

        // The store must accept anything the constructor produced and
        // round-trip the same speech back out via `recent`.
        let store = CheckpointStore::new(4, None);
        store.append(event.clone()).unwrap();
        let recent = store.recent(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].speech, event.speech);
    }
}

#[test]
fn fuzz_session_save_load_round_trip() {
    // 64 random sessions: build, persist via save_session_atomic, reload
    // via load_session, assert structural equality. Catches any drift in
    // the on-disk schema (e.g. a new field that doesn't deserialize back
    // to its default) that the unit tests miss because they only exercise
    // one shape.
    let mut seed = 0x5E55_2026_u64;
    let dir = unique_dir("aura-fuzz-session-roundtrip");
    fs::create_dir_all(&dir).unwrap();

    for i in 0..64 {
        let session_id = format!("conv-{i}-{}", next_u64(&mut seed) & 0xFFFF);
        let path = session_path(&dir, &session_id);

        let mut session = Session::new(
            &session_id,
            if next_u64(&mut seed).is_multiple_of(2) {
                Some(PathBuf::from(format!("/tmp/aura-fake-{i}.jsonl")))
            } else {
                None
            },
        );
        if !next_u64(&mut seed).is_multiple_of(3) {
            let intent = fuzz_string(&mut seed);
            let constraint = fuzz_string(&mut seed);
            session.record_envelope(TaskEnvelope::new(
                intent,
                vec![constraint],
                "aura-fuzz",
                CallbackMode::PingFirst,
                "approval",
            ));
        }
        if !next_u64(&mut seed).is_multiple_of(2) {
            session.record_user(&fuzz_string(&mut seed));
        }
        if !next_u64(&mut seed).is_multiple_of(2) {
            session.record_assistant(&fuzz_string(&mut seed));
        }
        if !next_u64(&mut seed).is_multiple_of(4) {
            session.record_attention_reason(&fuzz_string(&mut seed));
        }

        save_session_atomic(&path, &session).unwrap();
        let loaded = load_session(&path).unwrap().expect("session present");
        assert_eq!(loaded, session, "round-trip drift for {session_id}");
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn fuzz_build_recap_truncates_at_char_boundary() {
    // 256 random caps × random-shape sessions. Invariant: the recap is
    // ≤ max_chars AND ends on a UTF-8 char boundary so it's safe to
    // splice into Grok's `instructions` without panicking on `.truncate`
    // downstream. Em-dashes are 3 bytes, so they're the classic boundary
    // trap; we also throw in 4-byte emoji-shaped scalars to widen the
    // coverage.
    let mut seed = 0xB000_2026_u64;
    let multibyte_chars = ["\u{2014}", "\u{1F600}", "\u{00E9}", "\u{4E2D}"];

    for _ in 0..256 {
        let mut session = Session::new("fuzz", None);

        // Stuff multibyte runs into every recap field so the cap is
        // forced to land mid-scalar on most iterations.
        let pick = |s: &mut u64| multibyte_chars[(next_u64(s) as usize) % multibyte_chars.len()];
        let user_text = pick(&mut seed).repeat(60 + (next_u64(&mut seed) as usize % 200));
        session.record_user(&user_text);
        let asst_text = pick(&mut seed).repeat(40 + (next_u64(&mut seed) as usize % 200));
        session.record_assistant(&asst_text);
        let reason = pick(&mut seed).repeat(20 + (next_u64(&mut seed) as usize % 100));
        session.record_attention_reason(&reason);

        // max in [0, 1024] — include 0 to make sure the empty-cap path
        // is also safe.
        let max = (next_u64(&mut seed) as usize) % 1025;
        let recap = session.build_recap(max);

        assert!(
            recap.len() <= max,
            "build_recap exceeded cap: len={} max={max}",
            recap.len()
        );
        assert!(
            recap.is_char_boundary(recap.len()),
            "build_recap truncated mid-scalar at len={} max={max}",
            recap.len()
        );
    }
}

#[test]
fn fuzz_load_session_recovers_from_corruption() {
    // 256 random byte sequences written to disk and fed through
    // `load_session`. Invariants:
    //   - never panic (the whole point of corruption recovery),
    //   - return either Err (parse failed) or Ok(Some) when the bytes
    //     happen to form a valid Session,
    //   - never return Ok(None) — that is reserved for the
    //     ErrorKind::NotFound branch, which we never hit because the file
    //     always exists in this test.
    //
    // The corruption shapes the LCG explores are intentionally diverse:
    // raw garbage, JSON-shaped fragments, deeply-nested arrays, JSON
    // values that aren't sessions, and partial valid sessions with
    // missing or wrong-typed fields. They cover the surface the cycle 2
    // audit called out: structural drift, type drift, mid-rewrite
    // truncation, empty file, single-byte file, all-whitespace.
    let mut seed = 0xC07A_2026_u64;
    let dir = unique_dir("aura-fuzz-load-session-corruption");
    fs::create_dir_all(&dir).unwrap();

    for i in 0..256 {
        let path = dir.join(format!("corrupt-{i}.json"));
        let bytes = corrupt_payload(&mut seed, i);
        fs::write(&path, &bytes).unwrap();

        // The boundary contract: load_session must not panic regardless
        // of what's on disk. Catch unwind protects us from a regression
        // where some upstream change panics on malformed input.
        let outcome = std::panic::catch_unwind(|| load_session(&path));
        assert!(
            outcome.is_ok(),
            "load_session panicked on corruption case {i}: bytes={bytes:?}"
        );
        match outcome.unwrap() {
            Ok(Some(_)) => {
                // The bytes happened to form a valid Session — fine, it
                // round-tripped through serde without panicking.
            }
            Ok(None) => panic!(
                "load_session returned Ok(None) for an existing file (case {i}): bytes={bytes:?}"
            ),
            Err(_) => {
                // Expected: unparseable bytes return Err, not a panic.
            }
        }
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn fuzz_task_result_accepted_matches_handoff_state_invariant() {
    // `TaskResult::accepted()` is a derived function rather than a stored
    // field, so state and bool can never disagree:
    //
    //     pub fn accepted(&self) -> bool {
    //         matches!(self.handoff_state, TaskHandoffState::Accepted)
    //     }
    //
    // This is the wire-format contract Aura's task router and the
    // adapter speech-filter both depend on. Pin the contract: across
    // 64 random `TaskResult` shapes covering all three handoff states,
    // `accepted()` MUST equal `matches!(handoff_state, Accepted)` and
    // MUST NOT depend on any other field of the struct (intent, task
    // id, speech_update content, etc.).
    let mut seed = 0xACCE_2026_u64;
    for i in 0..64 {
        let state = pick_handoff_state(next_u64(&mut seed));
        let intent = fuzz_string(&mut seed);
        let constraint = fuzz_string(&mut seed);
        let speech = fuzz_string(&mut seed);
        let task_id = format!("task-fuzz-{i}");

        let envelope = TaskEnvelope::new(
            intent,
            vec![constraint],
            "aura-fuzz",
            CallbackMode::PingFirst,
            "approval",
        );
        let result = TaskResult {
            task_id: task_id.clone(),
            handoff_state: state.clone(),
            speech_update: speech,
            envelope,
        };

        let expected = matches!(state, TaskHandoffState::Accepted);
        assert_eq!(
            result.accepted(),
            expected,
            "accepted() diverged from handoff_state on case {i}: state={state:?} task_id={task_id}",
        );

        // Round-trip through JSON and assert the contract still holds —
        // catches a regression where `accepted` became a stored field
        // again by re-introducing disagreement on deserialization.
        let json = serde_json::to_string(&result).expect("TaskResult must be serializable");
        let reloaded: TaskResult =
            serde_json::from_str(&json).expect("TaskResult must round-trip through JSON");
        assert_eq!(
            reloaded.accepted(),
            expected,
            "accepted() after JSON round-trip diverged on case {i}",
        );
    }
}

fn pick_handoff_state(v: u64) -> TaskHandoffState {
    match v % 3 {
        0 => TaskHandoffState::Accepted,
        1 => TaskHandoffState::EnvelopePrepared,
        _ => TaskHandoffState::Rejected,
    }
}

fn corrupt_payload(seed: &mut u64, iteration: usize) -> Vec<u8> {
    // Cycle through a deliberate set of corruption shapes. Each shape is
    // reachable from the LCG so the test is fully deterministic.
    match iteration % 16 {
        0 => Vec::new(),                      // empty file
        1 => vec![0u8],                       // single null byte
        2 => vec![b'{'],                      // single brace
        3 => "   \n\t  ".as_bytes().to_vec(), // all whitespace
        4 => {
            // Random bytes including nulls and high-bit garbage.
            (0..(next_u64(seed) % 200) as usize)
                .map(|_| (next_u64(seed) & 0xFF) as u8)
                .collect()
        }
        5 => {
            // Truncated mid-object: open brace + some valid fragments
            // then chopped off, simulating a crash during a non-atomic
            // write before cycle 1's tmp-cleanup landed.
            let raw =
                serde_json::to_string_pretty(&Session::new(format!("session-{iteration}"), None))
                    .unwrap();
            let cap = (next_u64(seed) as usize) % raw.len().max(1);
            raw.as_bytes()[..cap].to_vec()
        }
        6 => {
            // Valid JSON but not an object — a top-level array.
            b"[1,2,3,4]".to_vec()
        }
        7 => {
            // Valid JSON, top-level number.
            b"3.14".to_vec()
        }
        8 => {
            // Valid JSON object missing all expected fields.
            b"{}".to_vec()
        }
        9 => {
            // Object with the right shape but field-type drift:
            // started_at_ms as a string instead of a number.
            br#"{"claude_session_id":"x","transcript_path":null,"started_at_ms":"not-a-number","last_active_at_ms":0,"last_envelope":null,"last_user_utterance":null,"last_assistant_utterance":null,"last_attention_reason":null,"recent_checkpoints":[]}"#
                .to_vec()
        }
        10 => {
            // Object missing required fields.
            br#"{"claude_session_id":"x"}"#.to_vec()
        }
        11 => {
            // Deeply nested array — does serde_json blow the stack? Cap
            // at 256 nesting so this stays fast and well under serde's
            // recursion limit (default 128 — anything past it returns
            // Err, which is the correct behavior).
            let depth = 10 + (next_u64(seed) as usize) % 246;
            let mut out = vec![b'['; depth];
            out.extend(std::iter::repeat_n(b']', depth));
            out
        }
        12 => {
            // Mixed BOM + garbage; tests that a UTF-8 BOM doesn't force
            // a panic upstream.
            let mut out = vec![0xEF, 0xBB, 0xBF];
            for _ in 0..(next_u64(seed) % 64) {
                out.push((next_u64(seed) & 0x7F) as u8);
            }
            out
        }
        13 => {
            // Valid session but with extra unexpected fields (forward
            // compatibility — should still parse).
            br#"{"claude_session_id":"x","transcript_path":null,"started_at_ms":0,"last_active_at_ms":0,"last_envelope":null,"last_user_utterance":null,"last_assistant_utterance":null,"last_attention_reason":null,"recent_checkpoints":[],"extra_future_field":42,"another":["x","y"]}"#
                .to_vec()
        }
        14 => {
            // ASCII printable garbage (often-overlooked corruption shape:
            // log fragments, partially-quoted strings).
            (0..(next_u64(seed) % 200) as usize)
                .map(|_| 32 + (next_u64(seed) % 95) as u8)
                .collect()
        }
        _ => {
            // JSON object with a `claude_session_id` field of the wrong
            // type (array instead of string) — should fail parse but not
            // panic.
            br#"{"claude_session_id":[1,2,3],"transcript_path":null,"started_at_ms":0,"last_active_at_ms":0,"last_envelope":null,"last_user_utterance":null,"last_assistant_utterance":null,"last_attention_reason":null,"recent_checkpoints":[]}"#
                .to_vec()
        }
    }
}

fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{label}-{nanos}"))
}

fn fuzz_string(seed: &mut u64) -> String {
    let len = (next_u64(seed) % 160) as usize;
    (0..len)
        .map(|_| {
            let byte = 32 + (next_u64(seed) % 95) as u8;
            byte as char
        })
        .collect()
}

fn next_u64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    *seed
}

fn pick_kind(value: u64) -> CheckpointKind {
    match value % 5 {
        0 => CheckpointKind::ToolUse,
        1 => CheckpointKind::Phase,
        2 => CheckpointKind::Error,
        3 => CheckpointKind::Note,
        _ => CheckpointKind::NeedsUserInput,
    }
}
