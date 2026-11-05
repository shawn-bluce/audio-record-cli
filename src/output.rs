//! Output directory resolution and file naming.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::ui;

const OUTPUT_DIR_ENV: &str = "AREC_OUTPUT_DIR";
const TS_FORMAT: &str = "%Y%m%d%H%M%S";
const JOURNAL_SUFFIX: &str = "_recording.aac";

/// Where recordings go: `$AREC_OUTPUT_DIR` if usable, else (with a warning)
/// the current working directory.
pub fn resolve_output_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("Cannot determine the current working directory")?;
    if let Some(raw) = std::env::var_os(OUTPUT_DIR_ENV).filter(|v| !v.is_empty()) {
        let wanted = cwd.join(expand_tilde(Path::new(&raw)));
        match ensure_writable(&wanted) {
            Ok(()) => return Ok(wanted),
            Err(e) => ui::warning(format_args!(
                "{OUTPUT_DIR_ENV} points to a directory that is not writable ({}: {e:#}); \
                 falling back to the current directory {}",
                wanted.display(),
                cwd.display()
            )),
        }
    }
    ensure_writable(&cwd)
        .with_context(|| format!("Output directory is not writable: {}", cwd.display()))?;
    Ok(cwd)
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_owned()
}

/// Create the directory if missing and prove we can create a file in it.
fn ensure_writable(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context("cannot create directory")?;
    let probe = dir.join(format!(".arec-write-test-{}", std::process::id()));
    fs::write(&probe, b"").context("cannot write a test file")?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// Local time as used in file names: `YYYYMMDDhhmmss`.
pub fn timestamp(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t.max(SystemTime::UNIX_EPOCH))
        .format(TS_FORMAT)
        .to_string()
}

/// Crash-safe stream written while recording (end time unknown yet).
pub fn journal_path(dir: &Path, start: &str) -> PathBuf {
    dir.join(format!("{start}{JOURNAL_SUFFIX}"))
}

/// Final recording: `<start>_<end>.m4a`.
pub fn final_path(dir: &Path, start: &str, end: &str) -> PathBuf {
    dir.join(format!("{start}_{end}.m4a"))
}

/// Copy of `recording` with long silences removed: `<start>_<end>_c.m4a`.
pub fn compact_path(recording: &Path) -> PathBuf {
    let stem = recording.file_stem().unwrap_or_default().to_string_lossy();
    recording.with_file_name(format!("{stem}_c.m4a"))
}

/// Start timestamp of a journal left behind by an interrupted recording.
pub fn journal_start(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let start = name.strip_suffix(JOURNAL_SUFFIX)?;
    (start.len() == 14 && start.bytes().all(|b| b.is_ascii_digit())).then(|| start.to_string())
}

/// Journals of interrupted recordings in `dir` (live ones are filtered out later by their lock).
pub fn find_journals(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| journal_start(p).is_some())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let d = Path::new("/tmp");
        let f = final_path(d, "20261105123455", "20261105125623");
        assert_eq!(f, PathBuf::from("/tmp/20261105123455_20261105125623.m4a"));
        assert_eq!(
            compact_path(&f),
            PathBuf::from("/tmp/20261105123455_20261105125623_c.m4a")
        );
        let j = journal_path(d, "20261105123455");
        assert_eq!(j, PathBuf::from("/tmp/20261105123455_recording.aac"));
        assert_eq!(journal_start(&j).as_deref(), Some("20261105123455"));
        assert_eq!(journal_start(Path::new("/tmp/x_recording.aac")), None);
    }

    #[test]
    fn unwritable_dir_rejected() {
        assert!(ensure_writable(Path::new("/System/arec-should-not-exist")).is_err());
    }
}
