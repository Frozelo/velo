use anyhow::{Result, bail};
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};
use std::{
    env,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const BUDGET: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: usize = 3;

pub struct Api {
    client: Client,
    keys: [String; 2],
    bases: [String; 2],
    mode: String,
    primary_retry: Option<Instant>,
    pub attempts: Vec<Value>,
}

pub struct Transcript {
    pub text: String,
    pub usage: Value,
    pub provider: &'static str,
    pub model: String,
}

fn key(service: &str, variable: &str) -> String {
    env::var(variable)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            Command::new("secret-tool")
                .args([
                    "lookup",
                    "application",
                    "voice-dictation",
                    "service",
                    service,
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        })
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("Dictation cancelled; no more requests will be sent")
    }
    Ok(())
}

fn wait_retry(delay: Duration, cancel: &AtomicBool) -> Result<()> {
    let until = Instant::now() + delay;
    while Instant::now() < until {
        check_cancel(cancel)?;
        thread::sleep(
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    }
    check_cancel(cancel)
}

fn network_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection"
    } else if error.is_body() {
        "response_body"
    } else if error.is_decode() {
        "invalid_json"
    } else {
        "transport"
    }
}

impl Api {
    pub fn new() -> Result<Self> {
        let mode = env::var("VELO_PROVIDER").unwrap_or_else(|_| "auto".into());
        if !["auto", "openrouter", "plusvibe"].contains(&mode.as_str()) {
            bail!("Invalid VELO_PROVIDER")
        }
        let keys = [
            key("openrouter", "OPENROUTER_API_KEY"),
            key("plusvibe", "PLUSVIBE_API_KEY"),
        ];
        if keys.iter().all(String::is_empty) {
            bail!("No API keys in Secret Service or environment")
        }
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(45))
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(1)
                .build()?,
            keys,
            bases: [
                "https://openrouter.ai/api/v1".into(),
                "https://plusvibeapi.ru/v1".into(),
            ],
            mode,
            primary_retry: None,
            attempts: Vec::new(),
        })
    }

    pub fn transcribe(&mut self, wav: &[u8], cancel: &AtomicBool) -> Result<Transcript> {
        self.attempts.clear();
        let started = Instant::now();
        let mut failure = "No key for selected provider".to_string();
        let skip_primary =
            self.primary_retry.is_some_and(|at| Instant::now() < at) && !self.keys[1].is_empty();
        for index in 0..2 {
            if self.keys[index].is_empty()
                || (index == 0
                    && (self.mode == "plusvibe" || (self.mode == "auto" && skip_primary)))
                || (index == 1 && self.mode == "openrouter")
            {
                continue;
            }
            let (provider, model) = if index == 0 {
                (
                    "openrouter",
                    env::var("VOICE_MODEL")
                        .unwrap_or_else(|_| "openai/whisper-large-v3-turbo".into()),
                )
            } else {
                (
                    "plusvibe",
                    env::var("VELO_PLUSVIBE_STT_MODEL")
                        .unwrap_or_else(|_| "whisper-large-v3-turbo".into()),
                )
            };
            let can_fallback = index == 0 && self.mode == "auto" && !self.keys[1].is_empty();
            for attempt in 1..=MAX_ATTEMPTS {
                check_cancel(cancel)?;
                let remaining = BUDGET.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    bail!("API retry budget exhausted; {failure}")
                }
                let attempt_start = Instant::now();
                let part = multipart::Part::bytes(wav.to_vec())
                    .file_name("recording.wav")
                    .mime_str("audio/wav")?;
                let form = multipart::Form::new()
                    .text("model", model.clone())
                    .text("language", "ru")
                    .part("file", part);
                let response = self
                    .client
                    .post(format!("{}/audio/transcriptions", self.bases[index]))
                    .bearer_auth(&self.keys[index])
                    .multipart(form)
                    .timeout(remaining.min(Duration::from_secs(30)))
                    .send();
                let mut status = None;
                let mut delay = Duration::from_millis(500 * attempt as u64);
                let mut server_delay_too_long = false;
                let (error_kind, retryable, fallback, uncertain) = match response {
                    Ok(response) => {
                        let code = response.status().as_u16();
                        status = Some(code);
                        if response.status().is_success() {
                            match response.json::<Value>() {
                                Ok(data) => {
                                    let text =
                                        data["text"].as_str().unwrap_or("").trim().to_string();
                                    let mut usage =
                                        data.get("usage").cloned().unwrap_or_else(|| json!({}));
                                    if index == 1
                                        && let Some(usage) = usage.as_object_mut()
                                        && let Some(cost) = usage.remove("cost")
                                    {
                                        usage.insert("provider_reported_cost".into(), cost);
                                    }
                                    self.attempts.push(json!({"provider": provider, "model":model, "attempt":attempt,
                                        "http_status":code, "seconds":attempt_start.elapsed().as_secs_f64(),
                                        "error_kind": if text.is_empty() { "empty_transcript" } else { "none" }, "usage":usage}));
                                    if text.is_empty() {
                                        bail!("{provider}: empty transcript; audio retained")
                                    }
                                    return Ok(Transcript {
                                        text,
                                        usage,
                                        provider,
                                        model,
                                    });
                                }
                                Err(error) => {
                                    let kind = network_kind(&error);
                                    (
                                        kind,
                                        error.is_timeout() || error.is_body(),
                                        can_fallback && (error.is_timeout() || error.is_body()),
                                        true,
                                    )
                                }
                            }
                        } else {
                            if let Some(value) =
                                response.headers().get(reqwest::header::RETRY_AFTER)
                            {
                                if let Some(seconds) =
                                    value.to_str().ok().and_then(|s| s.parse::<u64>().ok())
                                {
                                    delay = Duration::from_secs(seconds).max(delay);
                                    server_delay_too_long = seconds > 10;
                                } else {
                                    // HTTP-date is not parsed by the spike: do not retry earlier than requested.
                                    server_delay_too_long = true;
                                }
                            }
                            (
                                "http",
                                retry_status(code),
                                can_fallback && fallback_status(code),
                                code >= 500 || code == 408,
                            )
                        }
                    }
                    Err(error) => (network_kind(&error), true, can_fallback, true),
                };
                failure = format!(
                    "{provider}: {error_kind}{} (attempt {attempt})",
                    status.map(|s| format!(" HTTP {s}")).unwrap_or_default()
                );
                self.attempts.push(json!({"provider":provider,"model":model,"attempt":attempt,"seconds":attempt_start.elapsed().as_secs_f64(),
                    "http_status":status,"error_kind":error_kind,"cost_unknown":uncertain}));
                eprintln!("Velo native API: {failure}");
                if fallback {
                    self.primary_retry = Some(Instant::now() + Duration::from_secs(120));
                    break;
                }
                if !retryable
                    || attempt == MAX_ATTEMPTS
                    || server_delay_too_long
                    || started.elapsed() + delay >= BUDGET
                {
                    bail!("{failure}; audio retained")
                }
                wait_retry(delay, cancel)?;
            }
        }
        bail!("{failure}")
    }
}

fn retry_status(status: u16) -> bool {
    [408, 429].contains(&status) || status >= 500
}

fn fallback_status(status: u16) -> bool {
    [403, 408, 429, 451].contains(&status) || status >= 500
}
#[cfg(test)]
mod tests {
    #[test]
    fn regional_errors_fallback_but_bad_credentials_do_not() {
        assert!(super::fallback_status(403));
        assert!(super::fallback_status(502));
        assert!(!super::fallback_status(401));
        assert!(!super::fallback_status(400));
    }

    #[test]
    fn routes_to_backup_with_separate_key_and_remembers_primary_failure() {
        use std::{
            io::{BufRead, Read, Write},
            net::TcpListener,
            thread,
            time::Duration,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for code in [403, 200, 200] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut auth = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if lower.starts_with("authorization:") {
                        auth = line.trim().to_owned();
                    }
                    if lower.starts_with("content-length:") {
                        length = line
                            .split(':')
                            .nth(1)
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                requests.push((first, auth));
                let body = if code == 200 {
                    r#"{"text":"test","usage":{"cost":0.001}}"#
                } else {
                    "{}"
                };
                write!(socket, "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let mut api = super::Api {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            keys: ["primary-test-key".into(), "backup-test-key".into()],
            bases: [format!("{base}/primary"), format!("{base}/backup")],
            mode: "auto".into(),
            primary_retry: None,
            attempts: Vec::new(),
        };
        assert_eq!(
            api.transcribe(b"wav", &std::sync::atomic::AtomicBool::new(false))
                .unwrap()
                .provider,
            "plusvibe"
        );
        let second = api
            .transcribe(b"wav", &std::sync::atomic::AtomicBool::new(false))
            .unwrap();
        assert_eq!(second.provider, "plusvibe");
        assert!(second.usage.get("cost").is_none());
        assert!(second.usage.get("provider_reported_cost").is_some());
        let requests = server.join().unwrap();
        assert!(requests[0].0.contains("/primary/audio/transcriptions"));
        assert!(requests[0].1.contains("primary-test-key"));
        assert!(requests[1].0.contains("/backup/audio/transcriptions"));
        assert!(requests[1].1.contains("backup-test-key"));
        assert!(requests[2].0.contains("/backup/audio/transcriptions"));
    }

    fn local_api(
        codes: Vec<u16>,
        cancel_after_first: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> (super::Api, std::thread::JoinHandle<usize>) {
        use std::{
            io::{BufRead, Read, Write},
            net::TcpListener,
            thread,
            time::Duration,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut count = 0;
            for code in codes {
                listener.set_nonblocking(true).unwrap();
                let until = std::time::Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(s) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < until =>
                        {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(e) => panic!("Missing test request: {e}"),
                    }
                };
                count += 1;
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("content-length:") {
                        length = line
                            .split(':')
                            .nth(1)
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                if let Some(cancel) = &cancel_after_first {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if code == 0 {
                    continue;
                } // Simulate connection loss after upload.
                let body = if code == 200 {
                    r#"{"text":"test","usage":{}}"#
                } else {
                    "{}"
                };
                let retry = if code == 429 {
                    "Retry-After: 60\r\n"
                } else {
                    ""
                };
                write!(socket, "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            count
        });
        let api = super::Api {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            keys: [String::new(), "dummy-key".into()],
            bases: [base.clone(), base],
            mode: "plusvibe".into(),
            primary_retry: None,
            attempts: Vec::new(),
        };
        (api, server)
    }

    #[test]
    fn transient_failure_retries_and_preserves_attempt_diagnostics() {
        let (mut api, server) = local_api(vec![503, 200], None);
        let stop = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(api.transcribe(b"wav", &stop).unwrap().text, "test");
        assert_eq!(server.join().unwrap(), 2);
        assert_eq!(api.attempts.len(), 2);
        assert_eq!(api.attempts[0]["http_status"], 503);
        assert_eq!(api.attempts[0]["cost_unknown"], true);
        assert!(
            !serde_json::to_string(&api.attempts)
                .unwrap()
                .contains("dummy-key")
        );
    }

    #[test]
    fn dropped_connection_retries() {
        let (mut api, server) = local_api(vec![0, 200], None);
        assert!(
            api.transcribe(b"wav", &std::sync::atomic::AtomicBool::new(false))
                .is_ok()
        );
        assert_eq!(server.join().unwrap(), 2);
        assert!(api.attempts[0]["error_kind"].is_string());
    }

    #[test]
    fn bad_key_and_long_server_retry_delay_are_not_retried() {
        for code in [401, 429] {
            let (mut api, server) = local_api(vec![code], None);
            assert!(
                api.transcribe(b"wav", &std::sync::atomic::AtomicBool::new(false))
                    .is_err()
            );
            assert_eq!(server.join().unwrap(), 1);
            assert_eq!(api.attempts.len(), 1);
        }
    }

    #[test]
    fn cancelling_during_backoff_prevents_next_request() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut api, server) = local_api(vec![503], Some(cancel.clone()));
        let error = api.transcribe(b"wav", &cancel).err().expect("cancelled");
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(server.join().unwrap(), 1);
        assert_eq!(api.attempts.len(), 1);
    }
    #[test]
    fn retries_are_limited_to_three_requests() {
        let (mut api, server) = local_api(vec![503, 503, 503], None);
        assert!(
            api.transcribe(b"wav", &std::sync::atomic::AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(server.join().unwrap(), 3);
        assert_eq!(api.attempts.len(), 3);
    }
}
