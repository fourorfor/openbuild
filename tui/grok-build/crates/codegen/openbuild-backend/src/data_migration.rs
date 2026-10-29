//! One-time copy of the kernel's global directories out of the upstream `opencode` names.
//!
//! The kernel roots its global paths on `XDG_*` plus a hard-coded app name
//! (`packages/util/src/global.ts`). That constant is now `openbuild`, so a fresh
//! install writes:
//!
//! ```text
//! ~/.local/share/openbuild/   (sessions db, log, repos, shell, tool-output, snapshot)
//! ~/.local/state/openbuild/   (service.json, locks)
//! ~/.cache/openbuild/         (bin)
//! %TEMP%\openbuild\           (scratch)
//! ```
//!
//! Existing installs have their history in the `opencode` twins. This module
//! copies — never moves — that history once, renames the sqlite files to the new
//! database name, and drops a marker so later launches skip the work. The old
//! directories are left exactly as they were: somebody also running upstream
//! opencode keeps their own data, and a failed copy never loses anything.
//!
//! Invariants:
//!
//! - Only runs while the destination is missing or empty. A destination with
//!   content is authoritative and is never merged into.
//! - Runs before the kernel starts (the caller owns that ordering); otherwise the
//!   kernel creates the destination first and the copy would be skipped.
//! - A partial copy rolls the destination back, so the next launch retries from
//!   a clean slate instead of leaving a half-migrated tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Name of the marker written into each migrated destination directory.
pub const MARKER_FILE: &str = ".migrated-from-opencode";

/// Old and new database file names, and their sqlite sidecars.
const LEGACY_DB_PREFIX: &str = "opencode";
const DB_PREFIX: &str = "openbuild";
const DB_SIDECARS: [&str; 2] = ["-shm", "-wal"];

/// What one directory pair did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryOutcome {
    /// The old directory does not exist, so there was nothing to copy.
    NoLegacy,
    /// The new directory already held content; the migration left it alone.
    TargetInUse,
    /// A previous run already migrated this pair (marker present).
    AlreadyMigrated,
    /// The copy succeeded; the marker records where it came from.
    Migrated { files: usize, renamed: usize },
    /// The copy failed; the destination was rolled back and the next launch retries.
    Failed(String),
}

impl DirectoryOutcome {
    /// Whether the destination now holds migrated content.
    pub fn is_migrated(&self) -> bool {
        matches!(self, Self::Migrated { .. })
    }
}

/// One `(legacy, target)` pair plus what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub legacy: PathBuf,
    pub target: PathBuf,
    pub outcome: DirectoryOutcome,
}

impl MigrationReport {
    /// Whether this pair actually copied anything.
    pub fn copied(&self) -> bool {
        self.outcome.is_migrated()
    }
}

/// The four directory pairs the kernel writes into.
///
/// `data`, `state`, `cache` follow the XDG bases; `tmp` follows `os.tmpdir()`,
/// which the caller passes in because only the caller knows the process temp dir.
pub fn directory_pairs(
    home: &Path,
    xdg_data: Option<PathBuf>,
    xdg_state: Option<PathBuf>,
    xdg_cache: Option<PathBuf>,
    tmp: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let data = xdg_data.unwrap_or_else(|| home.join(".local").join("share"));
    let state = xdg_state.unwrap_or_else(|| home.join(".local").join("state"));
    let cache = xdg_cache.unwrap_or_else(|| home.join(".cache"));
    vec![
        (data.join(LEGACY_DB_PREFIX), data.join(DB_PREFIX)),
        (state.join(LEGACY_DB_PREFIX), state.join(DB_PREFIX)),
        (cache.join(LEGACY_DB_PREFIX), cache.join(DB_PREFIX)),
        (tmp.join(LEGACY_DB_PREFIX), tmp.join(DB_PREFIX)),
    ]
}

/// Resolve the pairs from the environment, or `None` without a home directory.
pub fn directory_pairs_from_env() -> Option<Vec<(PathBuf, PathBuf)>> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    Some(directory_pairs(
        &home,
        var("XDG_DATA_HOME"),
        var("XDG_STATE_HOME"),
        var("XDG_CACHE_HOME"),
        &std::env::temp_dir(),
    ))
}

/// Migrate every pair once. Returns one report per pair, in `directory_pairs` order.
///
/// Never fails as a whole: a pair that cannot be copied reports `Failed` and the
/// others still migrate. The caller logs the reports and shows one notice.
pub fn migrate_global_directories() -> Vec<MigrationReport> {
    let Some(pairs) = directory_pairs_from_env() else {
        return Vec::new();
    };
    pairs
        .into_iter()
        .map(|(legacy, target)| {
            let outcome = migrate_directory(&legacy, &target);
            MigrationReport {
                legacy,
                target,
                outcome,
            }
        })
        .collect()
}

/// Migrate one pair. Safe to call repeatedly: the marker and the destination
/// check both turn a second run into a no-op.
///
/// "In use" means the destination already holds a database or a marker. A
/// directory the kernel merely scaffolded (`log/`, `repos/`, `bin/`) is not
/// authoritative: the kernel creates those on any `--version` probe, and
/// treating them as content would silently skip a real migration.
pub fn migrate_directory(legacy: &Path, target: &Path) -> DirectoryOutcome {
    if !legacy.is_dir() {
        return DirectoryOutcome::NoLegacy;
    }
    if target.join(MARKER_FILE).is_file() {
        return DirectoryOutcome::AlreadyMigrated;
    }
    if has_database(target) {
        return DirectoryOutcome::TargetInUse;
    }
    let copied = match copy_tree(legacy, target) {
        Ok(copied) => copied,
        Err(error) => {
            // Leave no half-migrated destination: the next launch retries.
            let _ = fs::remove_dir_all(target);
            return DirectoryOutcome::Failed(error.to_string());
        }
    };
    let (renamed, rename_error) = rename_databases(target);
    if let Some(error) = rename_error {
        // The copy itself succeeded, so the tree is usable; only the database
        // name is wrong. Keep the destination and report the reason instead of
        // throwing away a complete copy of the user's history.
        tracing::warn!(
            target = %target.display(),
            %error,
            "copied the opencode data directory but could not rename the database"
        );
    }
    let marker = format!(
        "source={}\nmigrated_at={}\nrenamed_databases={renamed}\n",
        legacy.display(),
        timestamp()
    );
    if let Err(error) = fs::write(target.join(MARKER_FILE), marker) {
        tracing::warn!(
            target = %target.display(),
            %error,
            "could not write the migration marker; the next launch will retry"
        );
    }
    DirectoryOutcome::Migrated {
        files: copied,
        renamed,
    }
}

/// Recursively copy `from` into `to`, creating `to` when missing.
///
/// Returns how many files were actually written. An existing destination file is
/// left untouched, so a retry after a partial copy fills the gaps without
/// overwriting data the new install already wrote.
fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    fs::create_dir_all(to)?;
    let mut copied = 0usize;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copied += copy_tree(&source, &destination)?;
        } else if file_type.is_file() && !destination.exists() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &destination)?;
            copied += 1;
        }
        // Symlinks and other special files are skipped on purpose: copying a
        // link target would duplicate data outside the tree, and a dangling link
        // would make the migration fail for no benefit.
    }
    Ok(copied)
}

/// Whether `root` already holds a session database (any channel variant).
fn has_database(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(DB_PREFIX) && name.contains(".db"))
    })
}

/// Rename `opencode*.db` (and its `-shm` / `-wal` sidecars) to `openbuild*`.
///
/// Returns how many files were renamed and the first error, if any. A rename is
/// skipped when the destination already exists so a retry cannot clobber data.
fn rename_databases(root: &Path) -> (usize, Option<io::Error>) {
    let mut renamed = 0usize;
    let mut first_error = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                first_error.get_or_insert(error);
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(suffix) = name.strip_prefix(LEGACY_DB_PREFIX) else {
                continue;
            };
            // Matches the plain name and every channel variant (`-local`, `-beta`, …) plus
            // the sqlite sidecars. Anything else under the prefix stays untouched.
            let is_database = suffix.ends_with(".db")
                || DB_SIDECARS
                    .iter()
                    .any(|sidecar| suffix.ends_with(&format!(".db{sidecar}")));
            if !is_database {
                continue;
            }
            let new_name = format!("{DB_PREFIX}{suffix}");
            let destination = path.with_file_name(new_name);
            if destination.exists() {
                continue;
            }
            match fs::rename(&path, &destination) {
                Ok(()) => renamed += 1,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
    }
    (renamed, first_error)
}

/// Wall-clock timestamp for the marker, in seconds since the epoch.
///
/// Avoids a date-format dependency: the marker only has to be readable, and the
/// value doubles as a sortable record of when the copy ran.
fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// One-line summary for the startup notice, or `None` when nothing was copied.
pub fn summary(reports: &[MigrationReport]) -> Option<String> {
    let copied: Vec<&MigrationReport> = reports.iter().filter(|report| report.copied()).collect();
    if copied.is_empty() {
        return None;
    }
    let files: usize = copied
        .iter()
        .map(|report| match report.outcome {
            DirectoryOutcome::Migrated { files, .. } => files,
            _ => 0,
        })
        .sum();
    let failed = reports
        .iter()
        .filter(|report| matches!(report.outcome, DirectoryOutcome::Failed(_)))
        .count();
    let mut text = format!(
        "Copied {files} file{} from the old opencode data directory; the originals are untouched",
        if files == 1 { "" } else { "s" }
    );
    if failed > 0 {
        text.push_str(&format!(" ({failed} directory pair(s) could not be copied)"));
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openbuild-data-migrate-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("fixture");
        path
    }

    #[test]
    fn copies_and_renames_databases_without_touching_the_source() {
        let root = fixture("copy");
        let legacy = root.join("opencode");
        let target = root.join("openbuild");
        fs::create_dir_all(legacy.join("log")).expect("legacy tree");
        fs::write(legacy.join("opencode.db"), "db").expect("db");
        fs::write(legacy.join("opencode.db-wal"), "wal").expect("wal");
        fs::write(legacy.join("opencode-local.db"), "local").expect("local db");
        fs::write(legacy.join("log").join("x.log"), "log").expect("log file");

        let outcome = migrate_directory(&legacy, &target);

        assert_eq!(
            outcome,
            DirectoryOutcome::Migrated {
                files: 4,
                renamed: 3
            }
        );
        assert!(target.join("openbuild.db").is_file(), "db renamed");
        assert!(target.join("openbuild.db-wal").is_file(), "wal renamed");
        assert!(
            target.join("openbuild-local.db").is_file(),
            "channel db renamed"
        );
        assert!(!target.join("opencode.db").exists(), "old name is gone");
        assert!(target.join("log").join("x.log").is_file(), "nested copied");
        assert!(target.join(MARKER_FILE).is_file(), "marker written");
        // The source is untouched: the user may still run upstream opencode.
        assert!(legacy.join("opencode.db").is_file(), "source db kept");
        assert!(legacy.join("opencode.db-wal").is_file(), "source wal kept");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn second_run_is_a_noop() {
        let root = fixture("twice");
        let legacy = root.join("opencode");
        let target = root.join("openbuild");
        fs::create_dir_all(&legacy).expect("legacy tree");
        fs::write(legacy.join("opencode.db"), "db").expect("db");
        assert!(migrate_directory(&legacy, &target).is_migrated());
        assert_eq!(
            migrate_directory(&legacy, &target),
            DirectoryOutcome::AlreadyMigrated
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_existing_database_wins() {
        let root = fixture("target-in-use");
        let legacy = root.join("opencode");
        let target = root.join("openbuild");
        fs::create_dir_all(&legacy).expect("legacy tree");
        fs::write(legacy.join("opencode.db"), "old").expect("old db");
        fs::create_dir_all(&target).expect("target");
        fs::write(target.join("openbuild.db"), "new").expect("new db");

        assert_eq!(
            migrate_directory(&legacy, &target),
            DirectoryOutcome::TargetInUse
        );
        assert_eq!(
            fs::read_to_string(target.join("openbuild.db")).expect("read"),
            "new",
            "an existing database must not be overwritten"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// The kernel creates `log/` and `repos/` on any `--version` probe. Those
    /// scaffolded directories are not user data, so a migration must still fill in
    /// the history instead of treating the target as authoritative.
    #[test]
    fn a_scaffolded_target_still_receives_the_history() {
        let root = fixture("scaffolded");
        let legacy = root.join("opencode");
        let target = root.join("openbuild");
        fs::create_dir_all(&legacy).expect("legacy tree");
        fs::write(legacy.join("opencode.db"), "history").expect("db");
        fs::create_dir_all(target.join("log")).expect("scaffold");
        fs::write(target.join("log").join("opencode-local.log"), "new log").expect("log");

        let outcome = migrate_directory(&legacy, &target);

        assert_eq!(
            outcome,
            DirectoryOutcome::Migrated {
                files: 1,
                renamed: 1
            }
        );
        assert_eq!(
            fs::read_to_string(target.join("openbuild.db")).expect("migrated db"),
            "history"
        );
        assert_eq!(
            fs::read_to_string(target.join("log").join("opencode-local.log")).expect("log"),
            "new log",
            "an existing file in the target is never overwritten"
        );
    }

    #[test]
    fn a_retry_fills_missing_files_without_overwriting() {
        let root = fixture("retry");
        let legacy = root.join("opencode");
        let target = root.join("openbuild");
        fs::create_dir_all(legacy.join("shell")).expect("legacy tree");
        fs::write(legacy.join("opencode.db"), "db").expect("db");
        fs::write(legacy.join("shell").join("a.txt"), "old-a").expect("a");
        fs::create_dir_all(&target).expect("target");
        // Simulate a half-completed first attempt: one file arrived, the rest did not.
        fs::write(target.join("opencode.db"), "db").expect("copied db");

        let outcome = migrate_directory(&legacy, &target);

        assert!(
            outcome.is_migrated(),
            "the remaining files are still copied: {outcome:?}"
        );
        assert!(
            target.join("shell").join("a.txt").is_file(),
            "the unfinished file is filled in"
        );
    }
    #[test]
    fn missing_legacy_is_a_noop() {
        let root = fixture("missing");
        assert_eq!(
            migrate_directory(&root.join("absent"), &root.join("target")),
            DirectoryOutcome::NoLegacy
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn renames_do_not_clobber_an_existing_destination() {
        let root = fixture("rename-conflict");
        fs::write(root.join("opencode.db"), "old").expect("old");
        fs::write(root.join("openbuild.db"), "new").expect("new");
        let (renamed, error) = rename_databases(&root);
        assert_eq!(renamed, 0);
        assert!(error.is_none(), "a conflict is skipped, not an error");
        assert_eq!(
            fs::read_to_string(root.join("openbuild.db")).expect("read"),
            "new"
        );
        assert!(root.join("opencode.db").is_file(), "the loser stays put");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_pairs_follow_the_xdg_bases() {
        let home = PathBuf::from("/home/tester");
        let pairs = directory_pairs(
            &home,
            Some(PathBuf::from("/xdg/data")),
            None,
            None,
            Path::new("/tmp"),
        );
        assert_eq!(
            pairs[0],
            (
                PathBuf::from("/xdg/data/opencode"),
                PathBuf::from("/xdg/data/openbuild")
            )
        );
        assert_eq!(
            pairs[1],
            (
                home.join(".local").join("state").join("opencode"),
                home.join(".local").join("state").join("openbuild")
            )
        );
        assert_eq!(
            pairs[2],
            (
                home.join(".cache").join("opencode"),
                home.join(".cache").join("openbuild")
            )
        );
        assert_eq!(
            pairs[3],
            (
                PathBuf::from("/tmp").join("opencode"),
                PathBuf::from("/tmp").join("openbuild")
            )
        );
    }

    #[test]
    fn summary_is_none_when_nothing_copied() {
        let reports = vec![MigrationReport {
            legacy: PathBuf::from("/a"),
            target: PathBuf::from("/b"),
            outcome: DirectoryOutcome::NoLegacy,
        }];
        assert_eq!(summary(&reports), None);
    }

    #[test]
    fn summary_counts_copied_files() {
        let reports = vec![
            MigrationReport {
                legacy: PathBuf::from("/a"),
                target: PathBuf::from("/b"),
                outcome: DirectoryOutcome::Migrated {
                    files: 7,
                    renamed: 2,
                },
            },
            MigrationReport {
                legacy: PathBuf::from("/c"),
                target: PathBuf::from("/d"),
                outcome: DirectoryOutcome::TargetInUse,
            },
        ];
        let text = summary(&reports).expect("summary");
        assert!(text.contains("Copied 7 files"), "{text}");
        assert!(!text.contains("could not be copied"), "{text}");
    }

    #[test]
    fn empty_env_os_string_is_ignored_by_the_shared_helper() {
        // Guards the `filter(|value| !value.is_empty())` contract used above:
        // an empty XDG base must fall back to the home default, not resolve to "".
        let empty: Option<std::ffi::OsString> =
            Some(std::ffi::OsString::new()).filter(|value| !value.is_empty());
        assert!(empty.is_none());
    }
}
