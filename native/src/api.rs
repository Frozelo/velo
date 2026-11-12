use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, multipart};
use serde_json::Value;
use std::{
    env,
    process::Command,
    time::{Duration, Instant},
};

pub struct Api {
    client: Client,
    keys: [String; 2],
    mode: String,
    primary_retry: Option<Instant>,
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
            mode,
            primary_retry: None,
        })
    }

    pub fn transcribe(&mut self, wav: &[u8]) -> Result<Transcript> {
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
            let (provider, base, model) = if index == 0 {
                (
                    "openrouter",
                    "https://openrouter.ai/api/v1",
                    env::var("VOICE_MODEL")
                        .unwrap_or_else(|_| "openai/whisper-large-v3-turbo".into()),
                )
            } else {
                (
                    "plusvibe",
                    "https://plusvibeapi.ru/v1",
                    env::var("VELO_PLUSVIBE_STT_MODEL")
                        .unwrap_or_else(|_| "whisper-large-v3-turbo".into()),
                )
            };
            let part = multipart::Part::bytes(wav.to_vec())
                .file_name("recording.wav")
                .mime_str("audio/wav")?;
            let form = multipart::Form::new()
                .text("model", model.clone())
                .text("language", "ru")
                .part("file", part);
            let response = self
                .client
                .post(format!("{base}/audio/transcriptions"))
                .bearer_auth(&self.keys[index])
                .multipart(form)
                .send();
            let response = match response {
                Ok(r) => r,
                Err(_) => {
                    failure = format!("{provider}: network request failed (audio retained)");
                    if self.mode != "auto" || index != 0 {
                        bail!("{failure}")
                    }
                    self.primary_retry = Some(Instant::now() + Duration::from_secs(120));
                    continue;
                }
            };
            let status = response.status();
            if !status.is_success() {
                failure = format!("{provider}: HTTP {status}");
                if index == 0 && self.mode == "auto" && fallback_status(status.as_u16()) {
                    self.primary_retry = Some(Instant::now() + Duration::from_secs(120));
                    continue;
                }
                bail!("{failure}");
            }
            let data: Value = response.json().context("Invalid transcription response")?;
            let text = data["text"]
                .as_str()
                .context("Missing transcript")?
                .trim()
                .to_string();
            if text.is_empty() {
                bail!("{provider}: empty transcript (audio retained)")
            }
            let mut usage = data
                .get("usage")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if index == 1
                && let Some(usage) = usage.as_object_mut()
                && let Some(cost) = usage.remove("cost")
            {
                usage.insert("provider_reported_cost".into(), cost);
            }
            return Ok(Transcript {
                text,
                usage,
                provider,
                model,
            });
        }
        bail!("{failure}")
    }
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
}
