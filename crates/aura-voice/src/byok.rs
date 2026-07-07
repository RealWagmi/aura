//! Shared BYOK key resolution for the DIRECT providers: env var first, then
//! the OS keychain (service `"aura"`), with NO silent fallback — every failure
//! path is explicit. Each provider exposes a thin named wrapper
//! ([`crate::xai::resolve_xai_key`], [`crate::openai::resolve_openai_key`])
//! so call sites stay self-documenting.

use zeroize::Zeroizing;

use crate::VoiceError;

/// Keychain service name under which BYOK keys are stored. The entry/user name
/// is the env-var name (`XAI_API_KEY` / `OPENAI_API_KEY`), so one service
/// holds one entry per provider.
pub const KEYCHAIN_SERVICE: &str = "aura";

/// Resolve a BYOK key, wrapped in [`Zeroizing`] so the plaintext is wiped on
/// drop. The key is never placed in a struct with `Debug`/`Serialize`, a URL,
/// argv, or a log line.
///
/// Resolution order ("env → OS-keychain"):
///
/// 1. **Env** `env_var` (primary). Trimmed; an empty/whitespace value is
///    rejected, not used as a key.
/// 2. **OS keychain** (the [`keyring`] crate): service `"aura"`, entry named
///    like the env var. A non-empty secret is used (trimmed). The keychain
///    being *unavailable* (e.g. a headless Linux VPS with no
///    `org.freedesktop.secrets` service) or the entry being *absent* is NOT an
///    error by itself — it falls through to the typed error below. See
///    [`probe_keychain_key`].
/// 3. If neither source yields a key, a [`VoiceError::MissingKey`] is returned
///    explaining that BOTH sources were tried and how to set either. We never
///    panic and never return an empty key.
///
/// ## Headless-VPS story
///
/// On a headless server there is typically no secret-service daemon, so the
/// keychain probe in step 2 fails soft (it is caught and treated as "absent" —
/// no crash, no hang, no silently-empty key); **the env path is the supported
/// source there**.
pub fn resolve_key(
    env_var: &'static str,
    provider_label: &str,
) -> Result<Zeroizing<String>, VoiceError> {
    // 1. Env (primary; the only source that works on a headless VPS).
    if let Ok(k) = std::env::var(env_var) {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            return Ok(Zeroizing::new(trimmed.to_owned()));
        }
    }

    // 2. OS keychain — fails soft to "absent" (never an error by itself).
    if let Some(key) = probe_keychain_key(env_var) {
        return Ok(key);
    }

    // 3. Neither source yielded a usable key.
    Err(VoiceError::MissingKey(format!(
        "no BYOK {provider_label} key found: env {env_var} is unset/empty AND the OS keychain \
         (service \"{KEYCHAIN_SERVICE}\", entry \"{env_var}\") has no entry or is \
         unavailable. Set the env var (recommended on a headless server), e.g. \
         `export {env_var}=...`, or store it in the OS keychain under that \
         service/entry on a desktop."
    )))
}

/// Probe the OS keychain for a BYOK key. Returns `Some` only for a non-empty
/// secret; returns `None` for *every* "no key here" outcome — entry absent,
/// keychain locked, no secret-service daemon (headless Linux), construction
/// failure, or any other backend error. This is the soft-fail that makes step 2
/// of [`resolve_key`] safe on a headless VPS: it can neither hang nor panic,
/// and it never produces a silently-empty key.
///
/// The secret is wrapped in [`Zeroizing`] the instant it leaves the keychain so
/// no lingering plaintext `String` copy survives.
pub fn probe_keychain_key(entry_name: &str) -> Option<Zeroizing<String>> {
    // `Entry::new` itself is fallible (some platforms reject construction);
    // a failure here means "no usable keychain" → absent.
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, entry_name).ok()?;
    // Move the returned `String` straight into `Zeroizing` so the only copy of
    // the plaintext is the zeroizing one.
    let secret = Zeroizing::new(entry.get_password().ok()?);
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `trimmed` borrows `secret`; re-own into a fresh `Zeroizing` and let the
    // original (which may hold surrounding whitespace) drop/zeroize.
    Some(Zeroizing::new(trimmed.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses a DEDICATED fake env var so this test can never race the real
    /// `XAI_API_KEY`/`OPENAI_API_KEY` tests (env is process-global).
    #[test]
    fn resolve_key_env_first_trimmed_then_typed_error() {
        const VAR: &str = "AURA_TEST_BYOK_KEY";
        // env present (with surrounding whitespace) → trimmed env key wins.
        // SAFETY: this is the only test that touches this dedicated var.
        unsafe { std::env::set_var(VAR, "  sk-from-env  ") };
        let got = resolve_key(VAR, "test").expect("env-present should resolve");
        assert_eq!(got.as_str(), "sk-from-env");

        // env empty/whitespace → NOT used; falls to keychain (absent on CI for
        // this made-up entry) → typed MissingKey naming both sources.
        unsafe { std::env::set_var(VAR, "   ") };
        match resolve_key(VAR, "test") {
            Err(VoiceError::MissingKey(msg)) => {
                assert!(msg.contains(VAR));
                assert!(msg.contains(KEYCHAIN_SERVICE));
            }
            Ok(k) => panic!("whitespace env must not resolve, got {:?}", k.len()),
            Err(other) => panic!("expected MissingKey, got {other:?}"),
        }

        unsafe { std::env::remove_var(VAR) };
    }

    #[test]
    fn keychain_probe_is_soft_and_never_panics() {
        // The probe must return a value (Some/None) without hanging or
        // panicking regardless of whether a secret service exists. On headless
        // CI this exercises the unavailable/absent → None soft-fail.
        let probed = probe_keychain_key("AURA_TEST_BYOK_KEY");
        // None is the expected outcome for a made-up entry; a Some (paranoid
        // dev box) must be trimmed-nonempty. Both are acceptable.
        if let Some(k) = probed {
            assert!(!k.trim().is_empty());
        }
    }
}
