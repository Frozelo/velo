mod api;
mod audio;
mod linux;
mod ui;

use anyhow::{Context, Result, bail};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
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

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn save_recording(pcm: &[u8]) -> Result<PathBuf> {
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").expect("HOME")).join(".local/state"))
        .join("velo-native/sessions");
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let session = root.join(id.to_string());
    fs::create_dir(&session)?;
    fs::set_permissions(&session, fs::Permissions::from_mode(0o700))?;
    private_write(&session.join("audio.wav"), &audio::wav(pcm))?;
    Ok(session)
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
            let result: Result<()> = (|| {
                let target = linux::focused_window();
                let pcm = audio::capture(recording_stop.clone(), Duration::from_secs(120))?;
                let session = save_recording(&pcm)?;
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let _ = worker_events.send(Event::Phase(Phase::Processing));
                if client.is_none() {
                    client = Some(api::Api::new()?);
                }
                let transcript = client
                    .as_mut()
                    .unwrap()
                    .transcribe(&audio::wav(&pcm))
                    .with_context(|| format!("Recording retained at {}", session.display()))?;
                let result = serde_json::json!({"text": transcript.text, "provider": transcript.provider, "model": transcript.model, "usage": transcript.usage});
                private_write(&session.join("result.json"), &serde_json::to_vec(&result)?)?;
                fs::remove_file(session.join("audio.wav"))?;
                if !cancelled.load(Ordering::Relaxed) {
                    let pasted = linux::paste(result["text"].as_str().unwrap(), &target)?;
                    println!(
                        "Velo native: {} via {}",
                        if pasted {
                            "paste requested"
                        } else {
                            "copied only (focus changed)"
                        },
                        transcript.provider
                    );
                }
                Ok(())
            })();
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
        _ => println!(
            "Velo native integration spike\n  --doctor          input permissions\n  --capture-test 2  native microphone test, no API\n  --demo            capsule preview, no microphone or API\n  --hold            hold Right Cmd (existing keyd F24); release to transcribe and paste\n\nPython version remains the default. Native spike: batch STT, no editor, no history UI yet."
        ),
    }
    Ok(())
}
