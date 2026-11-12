mod api;
mod audio;
mod history;
mod linux;
mod sessions;
mod ui;

use anyhow::{Context, Result, bail};
use sessions::{private_write, save_recording};
use std::{
    env,
    fs::{self, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    Idle,
    Recording,
    Processing,
    Ready,
    Error,
}
pub enum Event {
    Down,
    Start,
    Phase(Phase),
    Error(String),
}

fn held() -> Result<()> {
    if Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "velo-hold.service"])
        .status()
        .is_ok_and(|s| s.success())
    {
        bail!(
            "Python listener is active. For the native test stop it with: systemctl --user stop velo-hold.service. Restore with: systemctl --user start velo-hold.service"
        );
    }
    linux::check_input()?;
    let runtime =
        PathBuf::from(env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?);
    let instance = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(runtime.join("velo-native.lock"))?;
    // Keep the descriptor alive until the application exits. No stale PID file.
    if unsafe { libc::flock(instance.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another native Velo instance is running");
    }
    let stop = Arc::new(AtomicBool::new(true));
    let cancel = Arc::new(AtomicBool::new(false));
    let (events, receiver) = mpsc::channel();
    let (commands, jobs) = mpsc::channel();
    let keyboard_events = events.clone();
    let keyboard_stop = stop.clone();
    thread::spawn(move || {
        let result = linux::listen(|down| {
            if down {
                keyboard_stop.store(false, Ordering::Relaxed);
                let _ = keyboard_events.send(Event::Down);
            } else {
                keyboard_stop.store(true, Ordering::Relaxed);
            }
        });
        if let Err(error) = result {
            let _ = keyboard_events.send(Event::Error(format!("Keyboard: {error:#}")));
        }
    });
    let worker_events = events.clone();
    let recording_stop = stop.clone();
    let cancelled = cancel.clone();
    thread::spawn(move || {
        let mut client = None;
        while jobs.recv().is_ok() {
            let mut saved_session = None;
            let mut report = serde_json::json!({"outcome":"failed", "stage":"capture"});
            let result: Result<()> = (|| {
                let target = linux::focused_window();
                report["target_window_id"] = target
                    .as_ref()
                    .and_then(|w| w.get("id"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let started = Instant::now();
                let pcm = audio::capture(recording_stop.clone(), Duration::from_secs(120))?;
                let captured = Instant::now();
                let recording = save_recording(&pcm)?;
                let session = recording.path.clone();
                saved_session = Some(recording);
                report["audio_seconds"] = serde_json::json!(pcm.len() as f64 / 32000.0);
                report["capture_seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
                if cancelled.load(Ordering::Relaxed) {
                    report["outcome"] = serde_json::json!("cancelled_before_api");
                    return Ok(());
                }
                let _ = worker_events.send(Event::Phase(Phase::Processing));
                report["stage"] = serde_json::json!("api");
                if client.is_none() {
                    client = Some(api::Api::new()?);
                }
                let api_started = Instant::now();
                let api = client.as_mut().unwrap();
                let response = api.transcribe(&audio::wav(&pcm), &cancelled);
                report["api_attempts"] = serde_json::json!(api.attempts);
                report["api_seconds"] = serde_json::json!(api_started.elapsed().as_secs_f64());
                let transcript = match response {
                    Ok(transcript) => transcript,
                    Err(_) if cancelled.load(Ordering::Relaxed) => {
                        report["outcome"] = serde_json::json!("cancelled_during_api");
                        return Ok(());
                    }
                    Err(error) => {
                        report["api_error"] = serde_json::json!(error.to_string());
                        return Err(error).with_context(|| {
                            format!("Recording retained at {}", session.display())
                        });
                    }
                };
                report["provider"] = serde_json::json!(transcript.provider);
                report["model"] = serde_json::json!(transcript.model);
                report["stage"] = serde_json::json!("delivery");
                let result = serde_json::json!({"text": transcript.text, "provider": transcript.provider, "model": transcript.model, "usage": transcript.usage});
                sessions::atomic_json(&session.join("result.json"), &result, true)?;
                fs::remove_file(session.join("audio.wav"))?;
                if !cancelled.load(Ordering::Relaxed) {
                    let delivery =
                        linux::paste(result["text"].as_str().unwrap(), &target, &cancelled)?;
                    report["delivery_reason"] = serde_json::json!(delivery.reason);
                    report["delivery_checked_window_ids"] =
                        serde_json::json!(delivery.checked_windows);
                    let pasted = delivery.pasted;
                    report["outcome"] = serde_json::json!(if delivery.reason == "cancelled" {
                        "cancelled_during_delivery"
                    } else if pasted {
                        "paste_requested"
                    } else {
                        "clipboard_only"
                    });
                    println!(
                        "Velo native: {} via {}",
                        if pasted {
                            "paste requested"
                        } else {
                            delivery.reason
                        },
                        transcript.provider
                    );
                } else {
                    report["outcome"] = serde_json::json!("cancelled_after_api");
                }
                report["capture_end_to_result_seconds"] =
                    serde_json::json!(captured.elapsed().as_secs_f64());
                Ok(())
            })();
            if let Some(session) = saved_session
                && let Err(error) = private_write(
                    &session.path.join("report.json"),
                    &serde_json::to_vec(&report).unwrap(),
                )
            {
                eprintln!("Native diagnostics could not be saved: {error}");
            }
            let _ = match result {
                Ok(()) => worker_events.send(Event::Phase(Phase::Ready)),
                Err(error) => worker_events.send(Event::Error(format!("{error:#}"))),
            };
        }
    });
    ui::run(receiver, commands, stop, cancel, false);
    Ok(())
}

fn main() -> Result<()> {
    // This executes before GTK initialization or any application thread exists.
    // A tiny Cairo capsule avoids allocating a GPU renderer by default.
    if env::var_os("GSK_RENDERER").is_none() {
        unsafe { env::set_var("GSK_RENDERER", "cairo") };
    }
    let args: Vec<_> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => println!("velo-native {}", env!("CARGO_PKG_VERSION")),
        Some("--doctor") => {
            linux::check_input()?;
            println!("GTK4: compiled; PipeWire: native backend; API keys are not read by doctor");
        }
        Some("--capture-test") => {
            let seconds: u64 = args.get(2).map(String::as_str).unwrap_or("2").parse()?;
            if !(1..=10).contains(&seconds) {
                bail!("Capture test must last 1–10 seconds")
            }
            let pcm = audio::capture(
                Arc::new(AtomicBool::new(false)),
                Duration::from_secs(seconds),
            )?;
            let n = pcm.len() / 2;
            let rms = (pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| f64::from(i16::from_le_bytes([p[0], p[1]])).powi(2))
                .sum::<f64>()
                / n as f64)
                .sqrt();
            println!(
                "Captured {:.2} seconds, mono S16LE, 16000 Hz; RMS {rms:.1}; audio discarded; no API requests",
                n as f64 / 16000.0
            );
        }
        Some("--demo") => {
            let (_send, receiver) = mpsc::channel();
            let (commands, _jobs) = mpsc::channel();
            ui::run(
                receiver,
                commands,
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
                true,
            );
        }
        Some("--hold") => held()?,
        Some("--history") => history::run()?,
        Some("--history-list") => {
            for entry in sessions::list(&sessions::root()?, sessions::HISTORY_LIMIT)? {
                println!(
                    "{}\t{}\ttext={}\taudio={}",
                    entry.id,
                    entry.outcome(),
                    entry.has_result,
                    entry.has_audio
                );
            }
        }
        _ => println!(
            "Velo native integration spike\n  --doctor          input permissions\n  --capture-test 2  native microphone test, no API\n  --demo            capsule preview, no microphone or API\n  --hold            hold Right Cmd (existing keyd F24); release to transcribe and paste\n  --history         saved native dictations; view and copy, no API\n  --history-list    list session metadata in the terminal, no text or API\n\nPython version remains the default. Native spike: batch STT, no editor or settings UI yet."
        ),
    }
    Ok(())
}
