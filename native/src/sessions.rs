use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const HISTORY_LIMIT: usize = 100;
const JSON_LIMIT: u64 = 1024 * 1024;

pub fn root() -> Result<PathBuf> {
    let state = match env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            PathBuf::from(env::var_os("HOME").context("HOME is required")?).join(".local/state")
        }
    };
    Ok(state.join("velo-native/sessions"))
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub struct Session {
    pub path: PathBuf,
    _lock: File,
}

/// The same lock is held by live dictation and recovery until their files are saved.
pub fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("session.lock"))?;
    if !file.metadata()?.is_file() {
        bail!("Некорректный файл блокировки сессии");
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Сессия уже обрабатывается. Дождись завершения и обнови историю.");
    }
    Ok(file)
}

pub fn save_recording(pcm: &[u8]) -> Result<Session> {
    let root = root()?;
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let session = root.join(id.to_string());
    fs::create_dir(&session)?;
    fs::set_permissions(&session, fs::Permissions::from_mode(0o700))?;
    let lock = lock(&session)?;
    private_write(&session.join("audio.wav"), &crate::audio::wav(pcm))?;
    Ok(Session {
        path: session,
        _lock: lock,
    })
}

/// Publish complete JSON on the same filesystem, with private permissions.
/// create=true refuses to replace an existing result, even a damaged one.
pub fn atomic_json(path: &Path, value: &Value, create: bool) -> Result<()> {
    let temporary = path.with_file_name(format!(
        ".{}-{}-{}.tmp",
        path.file_name().context("JSON filename")?.to_string_lossy(),
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let result = (|| {
        private_write(&temporary, &serde_json::to_vec(value)?)?;
        if create {
            fs::hard_link(&temporary, path)?;
        } else {
            fs::rename(&temporary, path)?;
        }
        File::open(path.parent().context("JSON directory")?)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

pub fn read_json(path: &Path) -> Result<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("Expected a regular JSON file");
    }
    read_json_file(file)
}

fn read_json_file(file: File) -> Result<Value> {
    let mut bytes = Vec::new();
    file.take(JSON_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > JSON_LIMIT {
        bail!("Saved JSON exceeds the 1 MiB limit");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn saved_text(session: &Path) -> Result<String> {
    let result =
        read_json(&session.join("result.json")).context("Could not read saved transcription")?;
    let text = result["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .context("Saved result contains no text")?;
    Ok(text.to_owned())
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
    pub report: Value,
    pub recovery: Value,
    pub has_result: bool,
    pub has_audio: bool,
    pub issue: Option<String>,
}

impl Entry {
    pub fn outcome(&self) -> &str {
        self.recovery["outcome"]
            .as_str()
            .or_else(|| self.report["outcome"].as_str())
            .unwrap_or("unknown")
    }

    pub fn text(&self) -> Result<String> {
        saved_text(&self.path)
    }
}

/// Read metadata only; transcription text is loaded when an entry is selected.
/// A partially written or older session must not hide the other recordings.
pub fn list(root: &Path, limit: usize) -> Result<Vec<Entry>> {
    let directories = match fs::read_dir(root) {
        Ok(directories) => directories,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for directory in directories {
        let directory = directory?;
        let id = directory.file_name().to_string_lossy().into_owned();
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(number) = id.parse::<u128>()
            && directory.file_type()?.is_dir()
        {
            paths.push((number, id, directory.path()));
        }
    }
    paths.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    paths.truncate(limit);
    let entries = paths
        .into_iter()
        .map(|(_, id, path)| {
            let report_path = path.join("report.json");
            let (report, issue) = match read_json(&report_path) {
                Ok(report) if report.is_object() => (report, None),
                Ok(_) => (Value::Null, Some("Некорректный формат отчёта".into())),
                Err(_) if !report_path.exists() => (Value::Null, None),
                Err(_) => (Value::Null, Some("Не удалось прочитать отчёт".into())),
            };
            let recovery_path = path.join("recovery.json");
            let (recovery, issue) = match read_json(&recovery_path) {
                Ok(recovery) if recovery.is_object() => (recovery, issue),
                Err(_) if !recovery_path.exists() => (Value::Null, issue),
                _ => (
                    Value::Null,
                    Some("Не удалось прочитать отчёт повторного распознавания".into()),
                ),
            };
            Entry {
                id,
                has_result: regular_file(&path.join("result.json")),
                has_audio: regular_file(&path.join("audio.wav")),
                path,
                report,
                recovery,
                issue,
            }
        })
        .collect();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "velo-history-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn session(&self, id: &str) -> PathBuf {
            let path = self.0.join(id);
            fs::create_dir(&path).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_latest_sessions_numerically_without_following_links() {
        let fixture = Fixture::new();
        fixture.session("9");
        fixture.session("10");
        fixture.session("100");
        fixture.session("other");
        symlink(fixture.0.join("100"), fixture.0.join("999")).unwrap();
        let entries = list(&fixture.0, 2).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["100", "10"]
        );
    }

    #[test]
    fn damaged_report_and_unfinished_session_do_not_hide_saved_text() {
        let fixture = Fixture::new();
        let done = fixture.session("1");
        private_write(&done.join("result.json"), br#"{"text":"Hello"}"#).unwrap();
        private_write(&done.join("report.json"), b"{incomplete").unwrap();
        let unfinished = fixture.session("2");
        private_write(&unfinished.join("audio.wav"), b"recording").unwrap();
        let entries = list(&fixture.0, 100).unwrap();
        assert!(entries[0].has_audio);
        assert!(!entries[0].has_result);
        assert!(entries[0].issue.is_none());
        assert!(entries[1].issue.is_some());
        assert_eq!(entries[1].text().unwrap(), "Hello");
    }

    #[test]
    fn invalid_result_never_becomes_copyable_text() {
        let fixture = Fixture::new();
        let path = fixture.session("1");
        private_write(&path.join("result.json"), br#"{"text":12}"#).unwrap();
        let entry = list(&fixture.0, 1).unwrap().remove(0);
        assert!(entry.text().is_err());
        fs::remove_file(path.join("result.json")).unwrap();
        symlink("report.json", path.join("result.json")).unwrap();
        private_write(&path.join("report.json"), br#"{"text":"not a result"}"#).unwrap();
        assert!(entry.text().is_err());
        assert!(!list(&fixture.0, 1).unwrap()[0].has_result);
    }

    #[test]
    fn oversized_json_is_rejected_and_missing_history_is_empty() {
        let fixture = Fixture::new();
        assert!(list(&fixture.0.join("missing"), 100).unwrap().is_empty());
        let path = fixture.session("1");
        private_write(
            &path.join("result.json"),
            &vec![b' '; JSON_LIMIT as usize + 1],
        )
        .unwrap();
        assert!(list(&fixture.0, 1).unwrap()[0].text().is_err());
    }

    #[test]
    fn publishing_a_result_never_overwrites_existing_data_or_leaves_temporary_files() {
        let fixture = Fixture::new();
        let path = fixture.0.join("result.json");
        atomic_json(&path, &serde_json::json!({"text":"original"}), true).unwrap();
        assert!(atomic_json(&path, &serde_json::json!({"text":"replacement"}), true).is_err());
        assert_eq!(read_json(&path).unwrap()["text"], "original");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}
