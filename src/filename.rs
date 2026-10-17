use chrono::NaiveDateTime;

pub const DEFAULT_TEMPLATE: &str = "Recording_{d}_{t}_{ms}";

pub const VARIABLES: [(&str, &str, &str, &str); 9] = [
    ("{d}", "%Y-%m-%d", "Date", "Дата"),
    ("{t}", "%H-%M-%S", "Time", "Время"),
    ("{y}", "%Y", "Year", "Год"),
    ("{m}", "%m", "Month", "Месяц"),
    ("{day}", "%d", "Day", "День"),
    ("{h}", "%H", "Hour", "Час"),
    ("{min}", "%M", "Minute", "Минута"),
    ("{s}", "%S", "Second", "Секунда"),
    ("{ms}", "%3f", "Milliseconds", "Миллисекунды"),
];

pub fn render(template: &str, now: NaiveDateTime, monitor: usize) -> String {
    let template = if template.trim().is_empty() {
        DEFAULT_TEMPLATE
    } else {
        template
    };
    let mut name = template.to_owned();
    for (tag, format, _, _) in VARIABLES {
        name = name.replace(tag, &now.format(format).to_string());
    }
    name = name.replace("{monitor}", &monitor.to_string());
    name = name
        .chars()
        .map(|ch| {
            if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    name = name.trim_matches([' ', '.']).to_owned();
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(".mp4"))
    {
        name.truncate(name.len() - 4);
    }
    // Leave room for the extension and collision suffix; count Windows UTF-16
    // units so non-BMP characters cannot exceed the component length limit.
    let mut units = 0;
    name = name
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= 120
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if name.is_empty() {
        return "Recording".to_owned();
    }
    let device = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let numbered = device
        .strip_prefix("COM")
        .or_else(|| device.strip_prefix("LPT"));
    if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered.is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    {
        name.insert(0, '_');
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-10-17 09:08:07.123", "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }
    #[test]
    fn expands_all_variables_without_confusing_month_and_minute() {
        assert_eq!(
            render(
                "{d}_{t}_{y}_{m}_{day}_{h}_{min}_{s}_{ms}_{monitor}",
                now(),
                2
            ),
            "2026-10-17_09-08-07_2026_10_17_09_08_07_123_2"
        );
    }
    #[test]
    fn filenames_cannot_escape_the_output_directory() {
        let name = render("../../folder\\clip:foo?*.MP4", now(), 1);
        assert_eq!(name, "_.._folder_clip_foo__");
        assert!(!name.contains(['/', '\\', ':']));
    }
    #[test]
    fn empty_templates_and_reserved_windows_names_are_usable() {
        assert_eq!(render(" ", now(), 1), "Recording_2026-10-17_09-08-07_123");
        assert_eq!(render(".mp4", now(), 1), "mp4");
        assert_eq!(render("...", now(), 1), "Recording");
        for name in ["CON", "nul.txt", "COM1", "lpt9", "COM¹"] {
            assert!(render(name, now(), 1).starts_with('_'));
        }
    }
    #[test]
    fn unicode_and_unknown_tags_are_preserved_and_long_names_are_bounded() {
        assert_eq!(render("Запись_{unknown}", now(), 1), "Запись_{unknown}");
        assert!(render(&"🎥".repeat(200), now(), 1).encode_utf16().count() <= 120);
    }
}
