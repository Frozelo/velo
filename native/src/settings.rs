use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{
    env,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    Auto,
    OpenRouter,
    PlusVibe,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Auto, Self::OpenRouter, Self::PlusVibe];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OpenRouter => "openrouter",
            Self::PlusVibe => "plusvibe",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "openrouter" => Ok(Self::OpenRouter),
            "plusvibe" => Ok(Self::PlusVibe),
            _ => bail!("Допустимые режимы провайдера: auto, openrouter, plusvibe"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => {
                "Сначала OpenRouter. При недоступности — PlusVibe, если для него сохранён ключ."
            }
            Self::OpenRouter => {
                "Распознавание только через OpenRouter. Автоматического перехода на PlusVibe нет."
            }
            Self::PlusVibe => {
                "Распознавание только через PlusVibe. Удобно, когда OpenRouter недоступен без VPN."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub provider: Provider,
    pub polish: bool,
}

pub fn path() -> Result<PathBuf> {
    let root = match env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").context("HOME is required")?).join(".config"),
    };
    Ok(root.join("velo-native/settings.json"))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("Не удалось открыть настройки Velo"),
        };
        if !file.metadata()?.is_file() {
            bail!("Настройки Velo должны быть обычным файлом");
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("Файл настроек Velo превышает 16 КиБ");
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("Некорректный JSON в настройках Velo")?;
        let object = value
            .as_object()
            .context("Настройки Velo должны содержать объект JSON")?;
        if object.get("version").and_then(|v| v.as_u64()) != Some(1) {
            bail!("Неподдерживаемая версия настроек Velo");
        }
        if object
            .keys()
            .any(|key| !["version", "provider", "polish"].contains(&key.as_str()))
        {
            bail!("В настройках Velo есть неизвестные поля; файл оставлен без изменений");
        }
        let provider = Provider::parse(
            object
                .get("provider")
                .and_then(|v| v.as_str())
                .context("В настройках Velo не указан провайдер")?,
        )?;
        let polish = match object.get("polish") {
            None => false,
            Some(value) => value
                .as_bool()
                .context("Параметр polish должен быть true или false")?,
        };
        Ok(Self { provider, polish })
    }

    pub fn save(self, path: &Path) -> Result<()> {
        let directory = path.parent().context("Settings directory")?;
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        crate::sessions::atomic_json(
            path,
            &json!({"version":1, "provider":self.provider.as_str(), "polish":self.polish}),
            false,
        )
    }
}

pub fn environment_override() -> Result<Option<String>> {
    environment_value("VELO_PROVIDER")
}

pub fn environment_value(variable: &str) -> Result<Option<String>> {
    match env::var(variable) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(_) => bail!("{variable} содержит некорректное значение"),
    }
}

pub fn parse_polish(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("VELO_POLISH должен быть 0 или 1"),
    }
}

pub fn config_at(path: &Path, provider: Option<&str>, polish: Option<&str>) -> Result<Config> {
    let mut config = if provider.is_some() && polish.is_some() {
        Config::default()
    } else {
        Config::load(path)?
    };
    if let Some(value) = provider {
        config.provider = Provider::parse(value).context("Некорректный VELO_PROVIDER")?;
    }
    if let Some(value) = polish {
        config.polish = parse_polish(value)?;
    }
    Ok(config)
}

pub fn effective_config() -> Result<Config> {
    config_at(
        &path()?,
        environment_override()?.as_deref(),
        environment_value("VELO_POLISH")?.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(env::temp_dir().join(format!(
                    "velo-settings-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )))
        }
        fn path(&self) -> PathBuf {
            self.0.join("settings.json")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saved_changes_are_seen_on_next_load_and_environment_takes_priority() {
        let f = Fixture::new();
        assert_eq!(config_at(&f.path(), None, None).unwrap(), Config::default());
        Config {
            provider: Provider::PlusVibe,
            polish: true,
        }
        .save(&f.path())
        .unwrap();
        assert_eq!(
            config_at(&f.path(), None, None).unwrap(),
            Config {
                provider: Provider::PlusVibe,
                polish: true
            }
        );
        assert_eq!(
            config_at(&f.path(), Some("openrouter"), Some("0")).unwrap(),
            Config {
                provider: Provider::OpenRouter,
                polish: false
            }
        );
        assert!(config_at(&f.path(), Some("typo"), None).is_err());
        assert!(config_at(&f.path(), None, Some("yes")).is_err());
        Config {
            provider: Provider::Auto,
            polish: false,
        }
        .save(&f.path())
        .unwrap();
        assert_eq!(config_at(&f.path(), None, None).unwrap(), Config::default());
        assert_eq!(
            fs::metadata(&f.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(f.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }

    #[test]
    fn invalid_files_fail_explicitly_and_are_not_rewritten_or_silently_ignored() {
        let f = Fixture::new();
        fs::create_dir(&f.0).unwrap();
        fs::write(f.path(), br#"{"version":1,"provider":"plusvibe"}"#).unwrap();
        assert!(
            !Config::load(&f.path()).unwrap().polish,
            "Older provider-only settings must remain valid"
        );
        for bytes in [
            b"{partial".as_slice(),
            br#"{"version":2,"provider":"auto"}"#,
            br#"{"version":1,"provider":"typo"}"#,
            br#"{"version":1,"provider":"auto","api_key":"do-not-store"}"#,
        ] {
            fs::write(f.path(), bytes).unwrap();
            assert!(config_at(&f.path(), None, None).is_err());
            assert_eq!(fs::read(f.path()).unwrap(), bytes);
            assert_eq!(
                config_at(&f.path(), Some("plusvibe"), Some("0"))
                    .unwrap()
                    .provider,
                Provider::PlusVibe
            );
        }
        fs::write(f.path(), vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(Config::load(&f.path()).is_err());
        fs::remove_file(f.path()).unwrap();
        std::os::unix::fs::symlink("missing", f.path()).unwrap();
        assert!(Config::load(&f.path()).is_err());
    }
}
