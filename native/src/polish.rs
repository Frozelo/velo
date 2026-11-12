use crate::{api, sessions};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

/// Caller holds the session lock. The original result is always preserved.
pub fn apply(session: &Path, api: &mut api::Api, cancel: &AtomicBool) -> Result<()> {
    run_with(session, cancel, |text, cancel, attempts| {
        let result = api.polish(text, cancel);
        *attempts = api.attempts.clone();
        result
    })
}

fn run_with(
    session: &Path,
    cancel: &AtomicBool,
    edit: impl FnOnce(&str, &AtomicBool, &mut Vec<Value>) -> Result<api::Transcript>,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let edited = session.join("edited.json");
    if fs::symlink_metadata(&edited).is_ok() {
        return Ok(());
    }
    let raw = sessions::saved_raw_text(session)?;
    let path = session.join("edit-report.json");
    let mut report = json!({"outcome":"edit_started", "api_attempts":[]});
    sessions::atomic_json(&path, &report, false)?;
    let started = Instant::now();
    let mut attempts = Vec::new();
    let result = edit(&raw, cancel, &mut attempts).and_then(|response| {
        if response.text.trim().is_empty() { bail!("Editor returned empty text; original retained"); }
        report["provider"] = json!(response.provider);
        report["model"] = json!(response.model);
        let value = json!({"text":response.text,"provider":response.provider,"model":response.model,"usage":response.usage});
        sessions::atomic_json(&edited, &value, true)?;
        Ok(())
    });
    report["api_attempts"] = json!(attempts);
    report["seconds"] = json!(started.elapsed().as_secs_f64());
    report["cancel_requested"] = json!(cancel.load(Ordering::Relaxed));
    report["outcome"] = json!(if result.is_ok() {
        "edited"
    } else if cancel.load(Ordering::Relaxed) {
        "edit_cancelled"
    } else {
        "edit_failed"
    });
    if let Err(error) = &result {
        report["error"] = json!(error.to_string());
    }
    sessions::atomic_json(&path, &report, false)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "velo-edit-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            sessions::atomic_json(
                &path.join("result.json"),
                &json!({"text":"привет как дела"}),
                true,
            )
            .unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn edited_and_original_text_survive_independently_and_cache_avoids_another_request() {
        let f = Fixture::new();
        run_with(&f.0, &AtomicBool::new(false), |text, _, attempts| {
            assert_eq!(text, "привет как дела");
            attempts.push(json!({"http_status":200}));
            Ok(api::Transcript {
                text: "Привет! Как дела?".into(),
                usage: json!({}),
                provider: "plusvibe",
                model: "test".into(),
            })
        })
        .unwrap();
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "Привет! Как дела?");
        assert_eq!(sessions::saved_raw_text(&f.0).unwrap(), "привет как дела");
        run_with(&f.0, &AtomicBool::new(false), |_, _, _| {
            panic!("cached edit sent twice")
        })
        .unwrap();
        fs::write(f.0.join("edited.json"), b"{damaged").unwrap();
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "привет как дела");
    }
    #[test]
    fn failure_empty_reply_and_cancellation_keep_raw_text_available() {
        let f = Fixture::new();
        assert!(
            run_with(&f.0, &AtomicBool::new(false), |_, _, attempts| {
                attempts.push(json!({"http_status":503,"cost_unknown":true}));
                bail!("test unavailable")
            })
            .is_err()
        );
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "привет как дела");
        let report = sessions::read_json(&f.0.join("edit-report.json")).unwrap();
        assert_eq!(report["outcome"], "edit_failed");
        assert_eq!(report["api_attempts"][0]["cost_unknown"], true);
        assert!(!report.to_string().contains("привет"));
        assert!(
            run_with(&f.0, &AtomicBool::new(false), |_, _, _| Ok(
                api::Transcript {
                    text: "  ".into(),
                    usage: json!({}),
                    provider: "plusvibe",
                    model: "test".into()
                }
            ))
            .is_err()
        );
        run_with(&f.0, &AtomicBool::new(true), |_, _, _| {
            panic!("cancelled edit sent")
        })
        .unwrap();
        assert!(!f.0.join("edited.json").exists());
        assert_eq!(sessions::saved_text(&f.0).unwrap(), "привет как дела");
    }
}
