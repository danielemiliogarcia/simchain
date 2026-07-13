//! `.env` reading, canonical rewriting, revision hashing and atomic,
//! ownership-preserving writes.
//!
//! The panel never preserves the original formatting of managed keys: on
//! apply it removes every managed line and recognized legacy alias and appends
//! one canonical block, so output is deterministic and duplicate keys cannot
//! linger. Legacy aliases remain valid migration inputs but are never emitted.

use sha2::{Digest, Sha256};
use simchain_common::live_tuning;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const MANAGED_BLOCK_HEADER: &str = "# Managed by simchain panel";
pub const ABSENT_REVISION: &str = "absent";

/// Unix ownership and mode to stamp on a written file, so a root panel
/// container never turns the host user's files into root-owned ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileOwnership {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Clone, Debug)]
pub struct EnvFileState {
    pub path: PathBuf,
    pub exists: bool,
    pub content: String,
    pub revision: String,
    pub ownership: Option<FileOwnership>,
}

pub fn revision_of(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Read the env file plus the metadata needed to write it back faithfully.
/// A missing file is a valid state (compose supplies every default).
pub fn read_env_file(path: &Path) -> std::io::Result<EnvFileState> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let metadata = fs::metadata(path)?;
            Ok(EnvFileState {
                path: path.to_path_buf(),
                exists: true,
                revision: revision_of(&content),
                ownership: Some(FileOwnership {
                    uid: metadata.uid(),
                    gid: metadata.gid(),
                    mode: metadata.mode() & 0o7777,
                }),
                content,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(EnvFileState {
            path: path.to_path_buf(),
            exists: false,
            content: String::new(),
            revision: ABSENT_REVISION.to_string(),
            ownership: None,
        }),
        Err(error) => Err(error),
    }
}

/// Key of a `KEY=value` line (tolerating `export KEY=value`), if it is one.
fn line_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Parse `KEY=value` pairs; later occurrences win, like compose interpolation.
pub fn parse_env(content: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in content.lines() {
        let Some(key) = line_key(line) else { continue };
        let trimmed = line.trim_start();
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((_, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        // Strip a trailing unquoted comment ("VALUE  # note"), like compose.
        let raw_value = raw_value.trim();
        let raw_value = if raw_value.starts_with('"') || raw_value.starts_with('\'') {
            raw_value
        } else {
            raw_value
                .split_once(" #")
                .map(|(v, _)| v)
                .unwrap_or(raw_value)
        };
        values.insert(key.to_string(), unquote(raw_value).trim().to_string());
    }
    values
}

/// The managed entries and recognized migration aliases currently present in
/// the file. Empty optional values are significant; empty required values use
/// their compose default. Aliases are retained in this in-memory source so the
/// shared spam parser applies exactly the standalone spammer's precedence and
/// conversion rules.
pub fn managed_overrides(content: &str) -> BTreeMap<String, String> {
    parse_env(content)
        .into_iter()
        .filter(|(key, value)| {
            live_tuning::spec(key).is_some_and(|spec| spec.optional || !value.trim().is_empty())
                || (live_tuning::is_legacy_alias(key) && !value.trim().is_empty())
        })
        .collect()
}

/// Legacy spam aliases present in the file. They are accepted as migration
/// inputs and converted to their canonical equivalents on the next write.
pub fn legacy_aliases_present(content: &str) -> Vec<String> {
    parse_env(content)
        .into_keys()
        .filter(|key| live_tuning::is_legacy_alias(key))
        .collect()
}

/// Rebuild the file: every unmanaged line verbatim (old managed block header
/// dropped), then one canonical managed block at the end.
pub fn render_with_managed_block(
    original: &str,
    managed: &BTreeMap<&'static str, String>,
) -> String {
    let mut kept: Vec<&str> = original
        .lines()
        .filter(|line| {
            if line.trim() == MANAGED_BLOCK_HEADER {
                return false;
            }
            match line_key(line) {
                Some(key) => {
                    !live_tuning::is_managed_key(key) && !live_tuning::is_legacy_alias(key)
                }
                None => true,
            }
        })
        .collect();
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }

    let mut output = String::new();
    for line in &kept {
        output.push_str(line);
        output.push('\n');
    }
    if !kept.is_empty() {
        output.push('\n');
    }
    output.push_str(MANAGED_BLOCK_HEADER);
    output.push('\n');
    // Catalog order, not BTreeMap order: keeps related settings together.
    for spec in live_tuning::MANAGED_SETTINGS {
        let value = managed.get(spec.key).cloned().unwrap_or_default();
        output.push_str(&format!("{}={}\n", spec.key, value));
    }
    output
}

/// Ownership for a brand-new file in `dir`: the directory's owner (the bind
/// mount preserves host uid/gid, so this is the host user even when the
/// panel runs as root).
pub fn dir_ownership(dir: &Path, mode: u32) -> std::io::Result<FileOwnership> {
    let metadata = fs::metadata(dir)?;
    Ok(FileOwnership {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode,
    })
}

/// Write atomically (temp file + rename in the same directory) and stamp the
/// given ownership/mode on the result.
pub fn write_atomic(path: &Path, content: &str, ownership: FileOwnership) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".panel-write.")
        .tempfile_in(dir)?;
    temp.write_all(content.as_bytes())?;
    temp.flush()?;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(ownership.mode))?;
    // Best-effort when unprivileged: chown to yourself always succeeds, and
    // inside the panel container we run as root so the host owner is applied.
    if let Err(error) =
        std::os::unix::fs::chown(temp.path(), Some(ownership.uid), Some(ownership.gid))
    {
        tracing::debug!(
            "chown {}:{} failed (non-root?): {error}",
            ownership.uid,
            ownership.gid
        );
    }
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = read_env_file(&dir.path().join(".env")).expect("read");
        assert!(!state.exists);
        assert_eq!(state.revision, ABSENT_REVISION);
        assert!(state.content.is_empty());
    }

    #[test]
    fn parse_handles_comments_quotes_and_inline_notes() {
        let content = "\
# a comment
BTC_IMAGE=bitcoin/bitcoin:31.1
FALLBACK_FEE=0.00015           # 15 sat/vB floor
QUOTED=\"hello world\"
export EXPORTED=yes
BROKEN LINE
EMPTY=
";
        let parsed = parse_env(content);
        assert_eq!(parsed["BTC_IMAGE"], "bitcoin/bitcoin:31.1");
        assert_eq!(parsed["FALLBACK_FEE"], "0.00015");
        assert_eq!(parsed["QUOTED"], "hello world");
        assert_eq!(parsed["EXPORTED"], "yes");
        assert_eq!(parsed["EMPTY"], "");
        assert!(!parsed.contains_key("BROKEN LINE"));
    }

    #[test]
    fn render_preserves_unmanaged_and_canonicalizes_managed() {
        let original = "\
# keep me
BTC_IMAGE=bitcoin/bitcoin:31.1
FALLBACK_FEE=0.00015
SPAM_TXS_PER_BLOCK=500

BLOCK_INTERVAL_MEAN_SECS=30
";
        let mut managed = BTreeMap::new();
        managed.insert("FALLBACK_FEE", "0.0002".to_string());
        managed.insert("BLOCK_INTERVAL_MEAN_SECS", "12".to_string());
        let rendered = render_with_managed_block(original, &managed);

        // Unmanaged lines survive verbatim; legacy aliases are migrated away.
        assert!(rendered.contains("# keep me\n"));
        assert!(rendered.contains("BTC_IMAGE=bitcoin/bitcoin:31.1\n"));
        assert!(!rendered.contains("SPAM_TXS_PER_BLOCK="));
        // Managed keys appear exactly once, in the canonical block.
        assert_eq!(rendered.matches("FALLBACK_FEE=").count(), 1);
        assert!(rendered.contains("FALLBACK_FEE=0.0002\n"));
        assert!(rendered.contains("BLOCK_INTERVAL_MEAN_SECS=12\n"));
        assert_eq!(rendered.matches(MANAGED_BLOCK_HEADER).count(), 1);
        // Every managed key is present in the block.
        for spec in live_tuning::MANAGED_SETTINGS {
            assert!(
                rendered.contains(&format!("{}=", spec.key)),
                "missing {}",
                spec.key
            );
        }
    }

    #[test]
    fn render_is_idempotent() {
        let mut managed = BTreeMap::new();
        managed.insert("FALLBACK_FEE", "0.0002".to_string());
        let once = render_with_managed_block("A=1\n", &managed);
        let twice = render_with_managed_block(&once, &managed);
        assert_eq!(once, twice);
    }

    #[test]
    fn write_atomic_applies_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".panel-token");
        let ownership = dir_ownership(dir.path(), 0o600).expect("dir ownership");
        write_atomic(&path, "secret", ownership).expect("write");
        let metadata = fs::metadata(&path).expect("metadata");
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).expect("read"), "secret");
    }

    #[test]
    fn atomic_replacement_preserves_existing_env_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");
        fs::write(&path, "A=1\n").expect("seed env");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("set mode");
        let before = read_env_file(&path)
            .expect("read metadata")
            .ownership
            .expect("ownership");
        write_atomic(&path, "A=2\n", before).expect("replace env");
        let after = fs::metadata(&path).expect("metadata");
        assert_eq!(after.uid(), before.uid);
        assert_eq!(after.gid(), before.gid);
        assert_eq!(after.mode() & 0o7777, before.mode);
    }

    #[test]
    fn legacy_aliases_are_detected() {
        let content = "SPAM_TXS_PER_BLOCK=500\nOTHER=1\n";
        assert_eq!(legacy_aliases_present(content), vec!["SPAM_TXS_PER_BLOCK"]);
    }

    #[test]
    fn empty_optional_override_is_preserved() {
        let overrides = managed_overrides(
            "BLOCK_INTERVAL_MIN_SECS=\nBLOCK_INTERVAL_MODE=\nFALLBACK_FEE=\nSPAM_TXS_PER_BLOCK=500\n",
        );
        assert_eq!(
            overrides.get("BLOCK_INTERVAL_MIN_SECS"),
            Some(&String::new())
        );
        assert_eq!(
            overrides.get("SPAM_TXS_PER_BLOCK").map(String::as_str),
            Some("500")
        );
        assert!(!overrides.contains_key("BLOCK_INTERVAL_MODE"));
        assert!(!overrides.contains_key("FALLBACK_FEE"));
    }
}
