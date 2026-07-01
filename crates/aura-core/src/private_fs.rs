//! Internal filesystem helpers shared by every aura-core module that
//! persists per-user state — `history`, `config`, `checkpoints`, and
//! `session`. All four writers want the same "private to the current
//! user" guarantee — 0o600 on files, 0o700 on directories — so the
//! implementation lives here once. The audit script (`audit_aura.sh`)
//! pins this contract by refusing to merge code that re-defines
//! `secure_dir` / `secure_file` in any of the four mutating-IO modules.
//!
//! On non-Unix targets the calls are no-ops; permissions there are
//! controlled by the parent ACL and we don't model that yet.
//!
//! ## TOCTOU contract
//!
//! - Directories materialise via `DirBuilderExt::mode` so `mkdir(2)`
//!   creates at 0o700 atomically; an existing dir keeps its mode and
//!   is verified non-symlink, non-file before we trust it.
//! - Leaf-file writers call `reject_symlink_for_write` before `open()`
//!   and add `O_NOFOLLOW` on supported Unix targets, so a pre-planted
//!   symlink at the leaf cannot route the write + post-open chmod onto
//!   an unrelated file.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

// `DirBuilder` is only used through `DirBuilderExt::mode` on Unix; on non-Unix
// the directory path falls back to `create_dir_all`, so importing it there
// would be an unused import.
#[cfg(unix)]
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

#[cfg(all(unix, target_os = "macos"))]
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;
#[cfg(all(unix, target_os = "linux"))]
const O_NOFOLLOW_FLAG: i32 = 0o400000;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
const O_NOFOLLOW_FLAG: i32 = 0;

/// Ensure `path` exists as a directory and is private to the current
/// user (`0o700`).
///
/// We deliberately do NOT chmod a pre-existing directory: if the user
/// pointed `history.path` (or similar) at a file in their project root,
/// then `path.parent()` is `.` — a directory that almost certainly
/// existed before Aura ran and that the user expects to keep its
/// regular `0o755` permissions. Silently locking down `.` would break
/// every other tool that relies on its parent directory being browsable.
///
/// Newly-created directories are materialized at 0o700 in a single
/// `mkdir(2)` syscall (via `DirBuilderExt::mode`) so there is no
/// chmod-after-create window for an attacker to peek through. If the
/// directory already exists (`EEXIST`) we treat that as success and
/// leave the mode untouched — same "respect user-managed dirs" policy
/// as before.
pub(crate) fn secure_dir(path: &Path) -> Result<(), String> {
    create_private_dir(path)
        .map_err(|err| format!("failed to create private dir {}: {err}", path.display()))
}

/// Create `path` (recursively if needed) as a private 0o700 directory.
/// Atomically distinguishes "we created it" (chmod-at-create via
/// `mode(0o700)`) from "it already existed" (no-op).
///
/// On `AlreadyExists` we additionally `symlink_metadata` the path and
/// reject it if it turns out to be a symlink or a regular file. The
/// kernel returns EEXIST whenever a name is already taken, regardless
/// of what inode kind sits there: a symlink would route subsequent
/// writes outside our state directory; a regular file would fail the
/// next child-open with a confusing ENOTDIR. We reject both up front
/// with a clear error message.
///
/// On non-Unix targets we fall back to `create_dir_all` because there
/// is no equivalent atomic "create with mode" primitive we want to
/// pull in a dep for; permissions on those targets are controlled by
/// the parent ACL anyway.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // `mkdir(2)` with EXPLICIT mode 0o700 in one syscall.
        // - Ok(()) means we created it; the kernel applied the mode
        //   honoring the process umask (umask masks bits OFF, never
        //   ON, so 0o700 stays 0o700 under any umask ≤ 0o077; under a
        //   wider umask like 0o022 the result is `0o700 & !0o022 =
        //   0o700` — still safe).
        // - ErrorKind::AlreadyExists means another writer (us in a
        //   prior run, the user, or a parallel process) created it
        //   first; we leave their mode intact, BUT we must verify it
        //   really is a directory (not a symlink or a regular file)
        //   before returning Ok.
        // - ErrorKind::NotFound means an ancestor is missing — we
        //   recurse on the parent first, then retry our own mkdir.
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => verify_real_dir(path),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        // Missing state ancestors are also Aura-owned,
                        // so create them through the same private-dir
                        // path. Pre-existing ancestors still keep their
                        // original mode through the AlreadyExists branch.
                        create_private_dir(parent)?;
                    }
                }
                match DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => verify_real_dir(path),
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// On the EEXIST path, confirm the existing inode is genuinely a
/// directory (not a symlink, not a regular file). We use
/// `symlink_metadata` rather than `metadata` because `metadata` follows
/// symlinks — the whole point is to catch the symlink case where the
/// link target IS a directory but the link itself is not what we want
/// to write through.
#[cfg(unix)]
fn verify_real_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure_dir: path already exists but is not a real directory (symlink or file)",
        ));
    }
    Ok(())
}

/// Reject `path` if it is a symlink to anything (file or directory).
///
/// State-file writers (`history.jsonl`, `checkpoints.jsonl`,
/// `sessions/*.json`, `config.json`) call this before chmodding the
/// leaf so a pre-planted symlink (e.g. `.aura/history.jsonl` linking
/// to `$HOME/.ssh/config`) cannot route the chmod + write onto an
/// unrelated file. We use `symlink_metadata` which does not follow
/// links.
///
/// Writers that go through `open_private` also set `O_NOFOLLOW` on
/// supported Unix targets, so the leaf cannot be swapped to a symlink
/// between the preflight check and the open. This function remains the
/// post-open permission normalization and a defensive symlink check for
/// existing callers.
pub(crate) fn secure_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        // `symlink_metadata` does NOT follow symlinks — that is the
        // whole point. A symlink resolved here would silently route
        // the chmod (and the caller's prior write) to whatever the
        // link pointed at; we refuse instead.
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "secure_file: refusing to chmod a symlink at {}",
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(format!(
                    "secure_file: target {} does not exist",
                    path.display()
                ));
            }
            Err(err) => {
                return Err(format!(
                    "secure_file: cannot stat {}: {err}",
                    path.display()
                ));
            }
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            format!(
                "failed to set private permissions on {}: {err}",
                path.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Reject `path` up front if it currently exists AND is a symlink.
/// Intended to be called by writers BEFORE they `open()` for create/
/// write/append, so a hostile pre-planted symlink can't route the open
/// to an unrelated file. Distinct from `secure_file` because the open-
/// time check needs to handle "does not exist yet" as the OK case.
///
/// TOCTOU window between check and open is acknowledged in the
/// `secure_file` doc comment above; same caveat applies here.
pub(crate) fn reject_symlink_for_write(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(format!(
                "refusing to write through a symlink at {}",
                path.display()
            )),
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!(
                "cannot stat write target {}: {err}",
                path.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Run the standard "prepare a private write target" preamble: ensure
/// the parent directory exists at 0o700 (creating it if missing,
/// leaving pre-existing dirs alone), then refuse if the leaf is
/// already a symlink. Skips the parent step when `path.parent()` is
/// empty (e.g. `history.jsonl` with cwd-relative path) — same policy
/// as the original `save_default_config` site, now applied uniformly.
fn prepare_private_write_target(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        secure_dir(parent)?;
    }
    reject_symlink_for_write(path)
}

// `opts` is only mutated inside the `#[cfg(unix)]` block below; on non-Unix it
// is used as-is, so the `mut` would be flagged unused there.
#[cfg_attr(not(unix), allow(unused_mut))]
fn open_private(path: &Path, mut opts: OpenOptions) -> Result<fs::File, String> {
    #[cfg(unix)]
    {
        opts.mode(0o600);
        if O_NOFOLLOW_FLAG != 0 {
            opts.custom_flags(O_NOFOLLOW_FLAG);
        }
    }
    opts.open(path)
        .map_err(|err| format!("failed to open private file {}: {err}", path.display()))
}

/// Append one serialized record as a single JSONL line. Used by
/// history.rs and checkpoints.rs — both want the same private-mode
/// guarantees + symlink rejection + post-open chmod.
///
/// `label` shows up in error messages so the operator can tell which
/// writer hit the failure (e.g. "history" vs "checkpoint log").
pub(crate) fn append_jsonl_line<T: serde::Serialize>(
    path: &Path,
    record: &T,
    label: &str,
) -> Result<(), String> {
    prepare_private_write_target(path)?;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    let mut file = open_private(path, opts).map_err(|err| format!("{label}: {err}"))?;
    secure_file(path)?;
    let raw = serde_json::to_string(record)
        .map_err(|err| format!("failed to serialize {label} event: {err}"))?;
    writeln!(file, "{raw}")
        .map_err(|err| format!("failed to write {label} to {}: {err}", path.display()))
}

pub fn append_private_jsonl_line<T: serde::Serialize>(
    path: &Path,
    record: &T,
    label: &str,
) -> Result<(), String> {
    append_jsonl_line(path, record, label)
}

/// Truncate-or-create + write a single payload, matching Site 3's
/// `save_default_config` ordering (chmod AFTER the write, not between
/// open and write — preserves the exact byte-for-byte behavior of the
/// pre-dedupe code).
///
/// Caller is responsible for serializing the payload; the helper
/// writes a trailing `\n` via `writeln!`.
pub(crate) fn write_private_truncated(
    path: &Path,
    payload: &str,
    label: &str,
) -> Result<(), String> {
    prepare_private_write_target(path)?;
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    let mut file = open_private(path, opts).map_err(|err| format!("{label}: {err}"))?;
    writeln!(file, "{payload}")
        .map_err(|err| format!("failed to write {label} to {}: {err}", path.display()))?;
    secure_file(path)
}

// Every test here exercises Unix permission semantics (0o600/0o700), so the
// whole module is Unix-only; on other targets it would be dead code with
// unused imports/helpers.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{label}-{nanos}"))
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_does_not_chmod_a_pre_existing_directory() {
        // The bug: pointing `history.path = "history.jsonl"` makes
        // `parent()` return `.`, and the old `secure_dir` would chmod
        // `.` to 0o700 — locking down the user's project directory.
        // After the fix, a directory that ALREADY existed must keep its
        // mode untouched.
        let dir = unique_dir("aura-secure-dir-existing");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let mode_before = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_before, 0o755);

        secure_dir(&dir).unwrap();

        let mode_after = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, 0o755,
            "secure_dir must not chmod a pre-existing directory"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_creates_a_new_dir_with_private_mode() {
        let dir = unique_dir("aura-secure-dir-new");
        assert!(!dir.exists(), "test setup: dir must not pre-exist");

        secure_dir(&dir).unwrap();

        assert!(dir.exists(), "secure_dir must create a missing directory");
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "newly-created private dirs must be chmod 0o700"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_creates_missing_state_ancestors_private() {
        // Helper recurses on a missing parent before retrying its
        // atomic 0o700 mkdir. Missing ancestors are also Aura-owned
        // state directories, so they must be private as well.
        let parent = unique_dir("aura-secure-dir-parent");
        let leaf = parent.join("inner").join("leaf");
        assert!(!parent.exists(), "test setup: parent must not pre-exist");

        secure_dir(&leaf).unwrap();

        assert!(leaf.exists(), "leaf must be created");
        let leaf_mode = fs::metadata(&leaf).unwrap().permissions().mode() & 0o777;
        assert_eq!(leaf_mode, 0o700, "leaf must be private 0o700");

        let parent_mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(parent_mode, 0o700, "missing ancestor must be private 0o700");
        let inner_mode = fs::metadata(parent.join("inner"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(inner_mode, 0o700, "inner ancestor must be private 0o700");
        let _ = fs::remove_dir_all(&parent);
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_rejects_pre_existing_regular_file() {
        // EEXIST can come back from a path that is a regular file,
        // not a directory. We catch the wrong-inode-type case at
        // secure_dir time with a clear error message rather than
        // letting the next child open fail with a confusing ENOTDIR.
        let path = unique_dir("aura-secure-dir-file");
        fs::write(&path, b"i am a regular file, not a dir").unwrap();
        let err = secure_dir(&path).expect_err("must reject regular file");
        assert!(
            err.contains("not a real directory"),
            "error must mention not a real directory, got {err:?}"
        );
        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_rejects_pre_existing_symlink() {
        // The symlink case is the dangerous one: writes routed through
        // `path` would resolve to the link target (potentially outside
        // our state directory). We use `symlink_metadata` (not
        // `metadata`) so we catch the link itself, not whatever it
        // points at.
        let target = unique_dir("aura-secure-dir-symtarget");
        fs::create_dir_all(&target).unwrap();
        let link = unique_dir("aura-secure-dir-symlink");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = secure_dir(&link).expect_err("must reject symlink");
        assert!(
            err.contains("not a real directory"),
            "error must mention not a real directory, got {err:?}"
        );
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&target);
    }

    #[cfg(unix)]
    #[test]
    fn secure_dir_does_not_clobber_dir_concurrently_created_by_another_writer() {
        // Surrogate for the TOCTOU race the round-3 fix closes: another
        // writer materialized the directory at 0o755 between the moment
        // we decided we needed to create it and our mkdir call. The new
        // code maps EEXIST to Ok(()) WITHOUT chmod-ing — i.e. we do not
        // mistake "already there" for "we made it" and lock down a dir
        // we don't own.
        let dir = unique_dir("aura-secure-dir-race");
        // Pre-create with a non-private mode, simulating the racing
        // writer that beat us to the punch.
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        secure_dir(&dir).unwrap();

        let mode_after = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode_after, 0o755,
            "EEXIST path must not chmod a directory we did not create"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
