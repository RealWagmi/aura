//! Conversation history — the append-only, speech-safe record of what
//! was said and done.
//!
//! Why this exists
//! ===============
//! Each [`HistoryEvent`] is sanitised at construction (`redact_secrets`
//! on the kind, `speech_safe_summary` on the speech), so the on-disk
//! JSONL and any in-memory replay are speech-safe by the time they
//! exist — a consumer can stream history into voice context without
//! re-filtering. Writes go through the private-FS JSONL appender;
//! reads are kernel-bounded (see `MAX_HISTORY_FILE_BYTES`) so a corrupt
//! or hostile oversized file fails loud instead of OOM-ing the bot.

use crate::private_fs::append_jsonl_line;
use crate::{redact_secrets, speech_safe_summary};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Maximum size of a history file we'll read into memory in one shot.
/// 200 events × ~50 KB each is a generous upper bound on legitimate
/// content; anything bigger is corruption or attack and should fail
/// loud rather than OOM the bot. Same kernel-bounded read pattern as
/// `aura-discord::read_reason`.
const MAX_HISTORY_FILE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub timestamp_ms: u128,
    pub kind: String,
    pub speech: String,
}

impl HistoryEvent {
    pub fn new(kind: impl Into<String>, speech: impl Into<String>) -> Self {
        Self {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            kind: redact_secrets(&kind.into()),
            speech: speech_safe_summary(&speech.into()),
        }
    }
}

pub fn append_history_event(path: &Path, event: &HistoryEvent) -> Result<(), String> {
    append_jsonl_line(path, event, "history")
}

pub fn load_history(path: &Path, max_events: usize) -> Result<Vec<HistoryEvent>, String> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to open history {}: {err}", path.display())),
    };
    // Kernel-bounded read: `take(MAX + 1)` makes the kernel stop us at
    // the byte budget regardless of how big the file grew between
    // open() and the BufRead pass below. We materialize the bytes
    // first so an oversized file is rejected before we spend time
    // parsing JSONL.
    let mut buf = Vec::with_capacity(64 * 1024);
    file.take(MAX_HISTORY_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| format!("failed to read history {}: {err}", path.display()))?;
    if buf.len() as u64 > MAX_HISTORY_FILE_BYTES {
        return Err(format!(
            "history {} exceeds {} byte cap (corruption or attack); refusing to load",
            path.display(),
            MAX_HISTORY_FILE_BYTES
        ));
    }
    let reader = BufReader::new(buf.as_slice());
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|err| format!("failed to read history: {err}"))?;
        if line.trim().is_empty() {
            continue;
        }
        // History is append-only local JSONL and can be torn by sync
        // tools or interrupted writes. A single bad line must not make
        // Aura history-blind; keep the readable events and leave the
        // file-size/read failures as the loud error cases.
        let Ok(event) = serde_json::from_str(&line) else {
            continue;
        };
        events.push(event);
    }
    if events.len() > max_events {
        Ok(events.split_off(events.len() - max_events))
    } else {
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn history_event_redacts_and_filters_speech() {
        let event = HistoryEvent::new(
            "API_KEY=abc12345678901234567890",
            "failed at src/auth.rs:217 with API_KEY=abc12345678901234567890",
        );
        assert!(!event.kind.contains("abc123"));
        assert!(!event.speech.contains("abc123"));
        assert!(!event.speech.contains("217"));
    }

    #[test]
    fn load_missing_history_is_empty() {
        let path = std::env::temp_dir().join("aura-missing-history-for-test.jsonl");
        let _ = fs::remove_file(&path);
        assert!(load_history(&path, 10).unwrap().is_empty());
    }

    #[test]
    fn load_history_skips_isolated_corrupt_lines() {
        let dir = std::env::temp_dir().join(format!(
            "aura-history-corrupt-lines-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp_ms\":1,\"kind\":\"user\",\"speech\":\"First\"}\n",
                "{ this is not json\n",
                "{\"timestamp_ms\":2,\"kind\":\"assistant\",\"speech\":\"Second\"}\n"
            ),
        )
        .unwrap();

        let events = load_history(&path, 10).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].speech, "First");
        assert_eq!(events[1].speech, "Second");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_history_rejects_oversized_file_without_panic_or_oom() {
        // Cycle 2 added a 64 KiB cap to `read_reason`; this is the
        // analogous guard for `load_history`. Anything above the
        // 10 MiB cap is corruption-or-attack and must fail closed
        // (return Err) rather than slurp the file or panic.
        //
        // We write `MAX + 1` bytes of `\n` (the cheapest valid byte for
        // BufRead::lines) so the file is over the cap but trivially
        // shaped. The test asserts:
        //   1. No panic — `read_to_end` with `take(MAX+1)` is bounded
        //      regardless of file size.
        //   2. Returns Err — the size check fires before any JSONL
        //      parsing kicks in.
        //   3. The error mentions the cap, so a future maintainer can
        //      grep for the message and find this code path quickly.
        let dir = std::env::temp_dir().join(format!(
            "aura-history-oversize-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        // 10 MiB + 1 byte: the smallest payload that trips the cap.
        let oversized = vec![b'\n'; 10 * 1024 * 1024 + 1];
        fs::write(&path, &oversized).unwrap();

        let outcome = std::panic::catch_unwind(|| load_history(&path, 100));
        assert!(outcome.is_ok(), "load_history panicked on oversized file");
        let result = outcome.unwrap();
        assert!(
            result.is_err(),
            "load_history must reject oversized file, got Ok({:?})",
            result.as_ref().map(Vec::len)
        );
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("byte cap") || err_msg.contains("exceeds"),
            "expected cap-related error, got: {err_msg}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_file_is_private() {
        let dir = std::env::temp_dir().join(format!(
            "aura-history-private-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history.jsonl");
        append_history_event(&path, &HistoryEvent::new("test", "Tests passed.")).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn append_history_event_refuses_to_follow_a_pre_planted_symlink() {
        // Threat: an attacker pre-creates `.aura/history.jsonl` as a
        // symlink pointing at `~/.ssh/config` (or any sensitive
        // file). Without the guard, every history append would write
        // JSONL through the link and `secure_file` would chmod the
        // link target to 0o600. We assert the writer fails up-front
        // AND leaves the link target byte-for-byte intact.
        let dir = std::env::temp_dir().join(format!(
            "aura-history-symlink-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target_dir = dir.join("victim");
        fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("config");
        let original_bytes = b"original victim content\n";
        fs::write(&target, original_bytes).unwrap();
        let original_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;

        let history_dir = dir.join(".aura");
        fs::create_dir_all(&history_dir).unwrap();
        let history_path = history_dir.join("history.jsonl");
        std::os::unix::fs::symlink(&target, &history_path).unwrap();

        let result =
            append_history_event(&history_path, &HistoryEvent::new("test", "Tests passed."));
        assert!(result.is_err(), "must refuse to write through symlink");
        let err = result.unwrap_err();
        assert!(
            err.contains("symlink"),
            "error must mention symlink, got: {err}"
        );

        let target_after = fs::read(&target).unwrap();
        assert_eq!(
            target_after, original_bytes,
            "victim file must not be touched"
        );
        let mode_after = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, original_mode,
            "victim file mode must not be chmodded"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
