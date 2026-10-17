use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::hotkeys::Shortcuts;
use crate::{
    i18n::{Language, Message},
    msg,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    pub monitor_index: usize,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub show_cursor: bool,
    pub system_audio: bool,
    pub microphone: bool,
    pub microphone_device: Option<String>,
    pub system_volume: f32,
    pub microphone_volume: f32,
    pub duration_minutes: u32,
    pub output_dir: PathBuf,
    pub logging_enabled: bool,
    pub hide_window: bool,
    pub shortcuts: Shortcuts,
}

impl Default for AppConfig {
    fn default() -> Self {
        let output_dir = directories::UserDirs::new()
            .and_then(|dirs| {
                dirs.video_dir()
                    .map(|path| path.join("hidden-screen-recorder"))
            })
            .unwrap_or_else(|| PathBuf::from("recordings"));

        Self {
            language: None,
            monitor_index: 1,
            fps: 30,
            bitrate_mbps: 12,
            show_cursor: true,
            system_audio: true,
            microphone: false,
            microphone_device: None,
            system_volume: 1.0,
            microphone_volume: 1.0,
            duration_minutes: 0,
            output_dir,
            logging_enabled: false,
            hide_window: true,
            shortcuts: Shortcuts::default(),
        }
    }
}

impl AppConfig {
    pub fn language(&self) -> Language {
        self.language.unwrap_or_else(Language::system)
    }
    fn path() -> Option<PathBuf> {
        ProjectDirs::from("dev", "", "hidden-screen-recorder")
            .map(|dirs| dirs.config_dir().join("settings.json"))
    }

    pub fn load() -> Result<(Self, Option<Message>), Message> {
        let path = Self::path().ok_or(msg!(
            "The settings directory is unavailable",
            "Каталог настроек недоступен"
        ))?;
        let legacy = ProjectDirs::from("dev", "ScreenRecord", "ScreenRecord")
            .map(|dirs| dirs.config_dir().join("settings.json"));
        let source = if path.exists() {
            path
        } else {
            legacy.filter(|p| p.exists()).unwrap_or(path)
        };
        Self::load_from(&source)
    }

    fn load_from(path: &Path) -> Result<(Self, Option<Message>), Message> {
        let json = match fs::read_to_string(path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(error) => {
                return Err(msg!(
                    "Could not read {}: {error}",
                    "Не удалось прочитать {}: {error}",
                    path.display()
                ));
            }
        };
        let mut config: Self = serde_json::from_str(&json).map_err(|error| {
            let backup = path.with_extension(format!(
                "corrupt-{}.json",
                chrono::Local::now().format("%Y%m%d%H%M%S%f")
            ));
            match fs::copy(path, &backup) {
                Ok(_) => msg!(
                    "Invalid settings: {error}. Backup: {}",
                    "Повреждены настройки: {error}. Копия: {}",
                    backup.display()
                ),
                Err(copy_error) => msg!(
                    "Invalid settings: {error}. Backup failed: {copy_error}",
                    "Повреждены настройки: {error}. Не удалось сохранить копию: {copy_error}"
                ),
            }
        })?;
        let before = serde_json::to_string(&config).map_err(|error| error.to_string())?;
        // Preserve documented advanced values and expose the same ranges in UI.
        config.fps = config.fps.clamp(1, 240);
        config.bitrate_mbps = config.bitrate_mbps.clamp(1, 100);
        config.system_volume = config.system_volume.clamp(0.0, 1.5);
        config.microphone_volume = config.microphone_volume.clamp(0.0, 1.5);
        config.duration_minutes = config.duration_minutes.min(24 * 60);
        if !config.shortcuts.is_valid() {
            config.shortcuts = Shortcuts::default();
        }
        if config.output_dir.as_os_str().is_empty() {
            config.output_dir = Self::default().output_dir;
        }
        let changed =
            before != serde_json::to_string(&config).map_err(|error| error.to_string())?;
        Ok((
            config,
            changed.then(|| {
                msg!(
                    "Invalid settings were corrected; check your preferences and shortcuts.",
                    "Некорректные настройки исправлены; проверьте значения и сочетания клавиш."
                )
            }),
        ))
    }

    pub fn save(&self) -> Result<(), Message> {
        let path = Self::path().ok_or(msg!(
            "The settings directory is unavailable",
            "Каталог настроек недоступен"
        ))?;
        self.save_to(&path).map_err(|error| {
            msg!(
                "Could not save {}: {error}",
                "Не удалось сохранить {}: {error}",
                path.display()
            )
        })
    }

    fn save_to(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().ok_or("Missing settings directory")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_save_replaces_existing_valid_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "previous contents").unwrap();
        let config = AppConfig {
            fps: 144,
            duration_minutes: 1000,
            ..Default::default()
        };
        config.save_to(&path).unwrap();
        let (loaded, warning) = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.fps, 144);
        assert_eq!(loaded.duration_minutes, 1000);
        assert!(warning.is_none());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_settings_are_reported_and_backed_up() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "{broken").unwrap();
        assert!(
            AppConfig::load_from(&path)
                .unwrap_err()
                .render(Language::Ru)
                .contains("Повреждены настройки")
        );
        let files: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|file| fs::read_to_string(file).unwrap() == "{broken")
        );
    }

    #[test]
    fn invalid_values_are_corrected_with_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, r#"{"fps":0,"bitrate_mbps":300,"duration_minutes":99999,"shortcuts":{"stop":{"key":120,"ctrl":true,"alt":false,"shift":false}}}"#).unwrap();
        let (loaded, warning) = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.fps, 1);
        assert_eq!(loaded.bitrate_mbps, 100);
        assert_eq!(loaded.duration_minutes, 1440);
        assert!(loaded.shortcuts.is_valid());
        assert!(warning.is_some());
    }

    #[test]
    fn failed_save_preserves_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "original").unwrap();
        let guard = fs::OpenOptions::new().read(true).open(&path).unwrap();
        assert!(AppConfig::default().save_to(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        drop(guard);
    }

    #[test]
    fn old_settings_keep_hidden_mode_and_disable_logging() {
        let config: AppConfig = serde_json::from_str(r#"{"fps":60,"microphone":true}"#).unwrap();
        assert_eq!(config.fps, 60);
        assert!(config.microphone);
        assert!(config.hide_window);
        assert!(!config.logging_enabled);
        assert_eq!(config.shortcuts, Shortcuts::default());
        assert_eq!(config.language, None);
        assert_eq!(config.language(), Language::system());
    }

    #[test]
    fn new_preferences_survive_serialization() {
        let config = AppConfig {
            logging_enabled: true,
            hide_window: false,
            shortcuts: Shortcuts {
                start: crate::hotkeys::Shortcut {
                    key: 0x52,
                    ctrl: true,
                    alt: false,
                    shift: true,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: AppConfig = serde_json::from_str(&json).unwrap();
        assert!(restored.logging_enabled);
        assert!(!restored.hide_window);
        assert_eq!(restored.shortcuts, config.shortcuts);
    }

    #[test]
    fn explicit_language_survives_save_and_overrides_system_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        for language in [Language::En, Language::Ru] {
            let config = AppConfig {
                language: Some(language),
                ..Default::default()
            };
            config.save_to(&path).unwrap();
            let (restored, warning) = AppConfig::load_from(&path).unwrap();
            assert_eq!(restored.language, Some(language));
            assert_eq!(restored.language(), language);
            assert!(warning.is_none());
        }
    }
}
