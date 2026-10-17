use serde::{Deserialize, Serialize};
use windows::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    Ru,
}

impl Language {
    pub fn system() -> Self {
        Self::from_windows_language(unsafe { GetUserDefaultUILanguage() })
    }

    fn from_windows_language(id: u16) -> Self {
        // Match every Russian sublanguage; unsupported system languages use English.
        if id & 0x03ff == 0x19 {
            Self::Ru
        } else {
            Self::En
        }
    }

    pub fn text<'a>(self, en: &'a str, ru: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::Ru => ru,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::En => Self::Ru,
            Self::Ru => Self::En,
        }
    }
}

/// Keep both translations so queued errors also follow a language change.
#[derive(Debug, Clone)]
pub struct Message {
    en: String,
    ru: String,
}

impl Message {
    pub fn new(en: impl Into<String>, ru: impl Into<String>) -> Self {
        Self {
            en: en.into(),
            ru: ru.into(),
        }
    }

    pub fn render(&self, language: Language) -> &str {
        language.text(&self.en, &self.ru)
    }

    pub fn detail(self, detail: impl std::fmt::Display) -> Self {
        Self::new(
            format!("{}: {detail}", self.en),
            format!("{}: {detail}", self.ru),
        )
    }

    pub fn join(messages: impl IntoIterator<Item = Self>) -> Option<Self> {
        let messages: Vec<_> = messages.into_iter().collect();
        if messages.is_empty() {
            return None;
        }
        Some(Self::new(
            messages
                .iter()
                .map(|m| m.en.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            messages
                .iter()
                .map(|m| m.ru.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

impl From<String> for Message {
    fn from(value: String) -> Self {
        Self::new(value.clone(), value)
    }
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.en)
    }
}

#[macro_export]
macro_rules! msg {
    ($en:literal, $ru:literal $(, $args:expr)* $(,)?) => {
        $crate::i18n::Message::new(format!($en $(, $args)*), format!($ru $(, $args)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_language_uses_russian_or_falls_back_to_english() {
        assert_eq!(Language::from_windows_language(0x0419), Language::Ru);
        assert_eq!(Language::from_windows_language(0x0819), Language::Ru);
        for id in [0, 0x0409, 0x0415, 0x0407] {
            assert_eq!(Language::from_windows_language(id), Language::En);
        }
    }

    #[test]
    fn queued_messages_change_language_without_losing_details() {
        let error = "device 42";
        let message = msg!("Microphone: {error}", "Микрофон: {error}");
        assert_eq!(message.render(Language::En), "Microphone: device 42");
        assert_eq!(message.render(Language::Ru), "Микрофон: device 42");
        assert_eq!(Language::Ru.toggled().toggled(), Language::Ru);
    }
}
