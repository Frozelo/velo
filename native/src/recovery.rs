use crate::{api, audio, sessions};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Cached,
    Recovered,
    Cancelled,
}

pub fn run(session: &Path, cancel: &AtomicBool) -> Result<Outcome> {
    run_with(session, cancel, |wav, cancel, attempts| {
        let mut api = api::Api::new()?;
        let result = api.transcribe(wav, cancel);
        *attempts = api.attempts;
        result
    })
}

fn read_audio(session: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(session.join("audio.wav"))
        .context("Не удалось открыть сохранённое аудио")?;
    if !file.metadata()?.is_file() {
        bail!("Аудиозапись должна быть обычным файлом");
    }
    let mut wav = Vec::new();
    file.take(audio::MAX_WAV_BYTES as u64 + 1)
        .read_to_end(&mut wav)?;
    audio::validate_wav(&wav)?;
    Ok(wav)
}

pub(crate) fn run_with(
    session: &Path,
    cancel: &AtomicBool,
    transcribe: impl FnOnce(&[u8], &AtomicBool, &mut Vec<Value>) -> Result<api::Transcript>,
) -> Result<Outcome> {
    if !fs::symlink_metadata(session)?.is_dir() {
        bail!("Некорректная папка сессии");
    }
    let _lock = sessions::lock(session)?;
    // An existing result is never replaced or sent to the API again. A partial
    // result from an older build must be kept for manual inspection as well.
    if fs::symlink_metadata(session.join("result.json")).is_ok() {
        sessions::saved_text(session)
            .context("Сохранённый результат не читается; исходные файлы оставлены без изменений")?;
        return Ok(Outcome::Cached);
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(Outcome::Cancelled);
    }
    let wav = read_audio(session)?;
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let journal = session.join(format!("recovery-{id}.json"));
    let summary = session.join("recovery.json");
    let mut report = json!({
        "id": id, "outcome": "recovery_started", "stage":"api",
        "audio_seconds": (wav.len() - 44) as f64 / 32000.0,
        "api_attempts": [],
    });
    // Persist the fact of an attempted recovery before sending anything. Each
    // run has its own journal so replacing the latest summary loses no attempts.
    sessions::atomic_json(&journal, &report, true)?;
    sessions::atomic_json(&summary, &report, false)?;
    let started = Instant::now();
    let mut attempts = Vec::new();
    let response = if cancel.load(Ordering::Relaxed) {
        Ok(None)
    } else {
        transcribe(&wav, cancel, &mut attempts).map(Some)
    };
    report["api_attempts"] = json!(attempts);
    report["api_seconds"] = json!(started.elapsed().as_secs_f64());
    report["cancel_requested"] = json!(cancel.load(Ordering::Relaxed));
    let result = (|| {
        let transcript = match response {
            Ok(Some(transcript)) => transcript,
            Ok(None) => return Ok(Outcome::Cancelled),
            Err(_) if cancel.load(Ordering::Relaxed) => return Ok(Outcome::Cancelled),
            Err(error) => return Err(error),
        };
        if transcript.text.trim().is_empty() {
            bail!("Сервис вернул пустой текст; аудио сохранено");
        }
        report["provider"] = json!(transcript.provider);
        report["model"] = json!(transcript.model);
        report["stage"] = json!("save");
        // Even if cancelled during an in-flight request, preserve its successful
        // response. Recovery never copies or injects text automatically.
        let result = json!({"text":transcript.text, "provider":transcript.provider,
            "model":transcript.model, "usage":transcript.usage});
        sessions::atomic_json(&session.join("result.json"), &result, true)
            .context("Не удалось сохранить текст; аудио оставлено для восстановления")?;
        Ok(Outcome::Recovered)
    })();
    report["outcome"] = json!(match &result {
        Ok(Outcome::Recovered) => "recovered",
        Ok(Outcome::Cancelled) => "recovery_cancelled",
        _ => "recovery_failed",
    });
    if let Err(error) = &result {
        report["error"] = json!(error.to_string());
    }
    sessions::atomic_json(&journal, &report, false)
        .context("Не удалось сохранить отчёт; файлы сессии оставлены на диске")?;
    sessions::atomic_json(&summary, &report, false)
        .context("Не удалось обновить отчёт; файлы сессии оставлены на диске")?;
    // Only a complete, synced result allows audio to be removed.
    if result
        .as_ref()
        .is_ok_and(|outcome| *outcome == Outcome::Recovered)
    {
        fs::remove_file(session.join("audio.wav"))
            .context("Текст сохранён, но удалить аудио не удалось")?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::atomic::AtomicUsize};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "velo-recovery-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            sessions::private_write(&path.join("audio.wav"), &audio::wav(&vec![0; 32000])).unwrap();
            sessions::private_write(
                &path.join("report.json"),
                br#"{"outcome":"failed","api_error":"old failure"}"#,
            )
            .unwrap();
            Self(path)
        }
        fn run(&self, cancel: &AtomicBool, text: &str) -> Result<Outcome> {
            run_with(&self.0, cancel, |_, _, attempts| {
                attempts.push(json!({"provider":"plusvibe", "http_status":200}));
                Ok(api::Transcript {
                    text: text.into(),
                    usage: json!({}),
                    provider: "plusvibe",
                    model: "test-model".into(),
                })
            })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recovery_saves_result_and_keeps_original_failure_report() {
        let f = Fixture::new();
        assert_eq!(
            f.run(&AtomicBool::new(false), "Привет!").unwrap(),
            Outcome::Recovered
        );
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "Привет!");
        assert!(!f.0.join("audio.wav").exists());
        assert_eq!(
            sessions::read_json(&f.0.join("report.json")).unwrap()["outcome"],
            "failed"
        );
        let report = sessions::read_json(&f.0.join("recovery.json")).unwrap();
        assert_eq!(report["outcome"], "recovered");
        assert_eq!(report["api_attempts"][0]["http_status"], 200);
        assert_eq!(
            fs::metadata(f.0.join("result.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn cached_result_and_locked_session_make_no_request() {
        let f = Fixture::new();
        let lock = sessions::lock(&f.0).unwrap();
        assert!(
            run_with(&f.0, &AtomicBool::new(false), |_, _, _| panic!(
                "locked request sent"
            ))
            .is_err()
        );
        drop(lock);
        f.run(&AtomicBool::new(false), "cached").unwrap();
        let outcome = run_with(&f.0, &AtomicBool::new(false), |_, _, _| {
            panic!("API called twice")
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Cached);
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "cached");
    }

    #[test]
    fn failure_retains_audio_and_each_run_has_a_separate_journal() {
        let f = Fixture::new();
        for _ in 0..2 {
            assert!(
                run_with(&f.0, &AtomicBool::new(false), |_, _, attempts| {
                    attempts.push(json!({"http_status":503, "cost_unknown":true}));
                    bail!("test network failure")
                })
                .is_err()
            );
        }
        assert!(f.0.join("audio.wav").exists());
        assert!(!f.0.join("result.json").exists());
        let journals: Vec<_> = fs::read_dir(&f.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("recovery-"))
            .collect();
        assert_eq!(journals.len(), 2);
        let report = sessions::read_json(&f.0.join("recovery.json")).unwrap();
        assert_eq!(report["outcome"], "recovery_failed");
        assert_eq!(report["api_attempts"][0]["cost_unknown"], true);
    }

    #[test]
    fn cancelled_before_request_or_during_request_preserves_recoverable_data() {
        let f = Fixture::new();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            run_with(&f.0, &cancelled, |_, _, _| panic!("cancelled request sent")).unwrap(),
            Outcome::Cancelled
        );
        cancelled.store(false, Ordering::Relaxed);
        let outcome = run_with(&f.0, &cancelled, |_, cancel, _| {
            cancel.store(true, Ordering::Relaxed);
            bail!("cancelled in flight")
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(f.0.join("audio.wav").exists());
        cancelled.store(false, Ordering::Relaxed);
        let outcome = run_with(&f.0, &cancelled, |_, cancel, _| {
            cancel.store(true, Ordering::Relaxed);
            Ok(api::Transcript {
                text: "finished before cancel".into(),
                usage: json!({}),
                provider: "plusvibe",
                model: "test".into(),
            })
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Recovered);
        assert_eq!(
            sessions::saved_text(&f.0).unwrap(),
            "finished before cancel"
        );
    }

    #[test]
    fn damaged_wav_or_existing_damaged_result_is_never_sent_or_overwritten() {
        let f = Fixture::new();
        fs::write(f.0.join("audio.wav"), b"truncated WAV").unwrap();
        let calls = AtomicUsize::new(0);
        let result = run_with(&f.0, &AtomicBool::new(false), |_, _, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            bail!("should not send")
        });
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        sessions::private_write(&f.0.join("result.json"), b"{partial").unwrap();
        assert!(f.run(&AtomicBool::new(false), "overwrite").is_err());
        assert_eq!(fs::read(f.0.join("result.json")).unwrap(), b"{partial");
    }
}
