//! The command palette: everything the main window can do, including appearance,
//! found by typing. Entries carry their shortcut or current value on the right.

/// Themes the palette offers; the index is the persisted palette id.
pub const THEMES: [&str; 3] = ["Default", "Black", "Graphite"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    /// The shortcut, or the current value.
    pub detail: String,
}

/// What the entries reflect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    pub palette: i32,
    pub interface_percent: i32,
    pub terminal_percent: i32,
}

fn entry(id: impl Into<String>, title: impl Into<String>, detail: impl Into<String>) -> Entry {
    Entry { id: id.into(), title: title.into(), detail: detail.into() }
}

pub fn entries(current: Current) -> Vec<Entry> {
    let terminal = format!("{}%", current.terminal_percent);
    let interface = format!("{}%", current.interface_percent);
    let mut entries = vec![
        entry("new-workspace", "New Session", "Ctrl+Shift+N"),
        entry("new-pane-auto-layout", "New Terminal", "Ctrl+Shift+T"),
        entry("close-terminal", "Close Terminal", "Ctrl+Shift+W"),
        entry("toggle-sidebar", "Toggle Sidebar", "Ctrl+Shift+B"),
        entry("new-profile", "New Profile", ""),
        entry("terminal-larger", "Terminal Text Larger", format!("Ctrl+=  {terminal}")),
        entry("terminal-smaller", "Terminal Text Smaller", format!("Ctrl+-  {terminal}")),
        entry("terminal-reset", "Terminal Text Reset", format!("Ctrl+0  {terminal}")),
        entry("interface-larger", "Interface Text Larger", interface.clone()),
        entry("interface-smaller", "Interface Text Smaller", interface.clone()),
        entry("interface-reset", "Interface Text Reset", interface),
    ];
    for (index, name) in THEMES.iter().enumerate() {
        let current = if index as i32 == current.palette { "current" } else { "" };
        entries.push(entry(format!("theme:{index}"), format!("Theme: {name}"), current));
    }
    entries.extend([
        entry("reset-appearance", "Reset Appearance", ""),
        entry("layout-equal", "Equal Layout", ""),
        entry("layout-columns", "Side by Side", ""),
        entry("layout-rows", "Stacked Layout", ""),
        entry("layout-restore", "Restore Previous Layout", ""),
        entry("toggle-dock", "Toggle Dock", ""),
    ]);
    entries
}

/// Entries whose title holds every typed word, in order of how early the words match;
/// a word may also match the initials of the title ("tts" finds "Terminal Text
/// Smaller").
pub fn matching(entries: Vec<Entry>, query: &str) -> Vec<Entry> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return entries;
    }
    let mut found: Vec<(usize, usize, Entry)> = entries
        .into_iter()
        .enumerate()
        .filter_map(|(order, entry)| {
            let title = entry.title.to_lowercase();
            let initials: String = title.split(|c: char| !c.is_alphanumeric()).filter_map(|word| word.chars().next()).collect();
            let mut score = 0;
            for word in &words {
                match title.find(word.as_str()) {
                    Some(at) => score += at,
                    None if initials.starts_with(word.as_str()) => score += 100,
                    None => return None,
                }
            }
            Some((score, order, entry))
        })
        .collect();
    found.sort_by_key(|(score, order, _)| (*score, *order));
    found.into_iter().map(|(_, _, entry)| entry).collect()
}

/// The next text size in steps of ten, within 50–200%.
pub fn step(percent: i32, larger: bool) -> i32 {
    crate::typography::normalize_percent(percent + if larger { 10 } else { -10 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: Current = Current { palette: 1, interface_percent: 100, terminal_percent: 120 };

    #[test]
    fn entries_show_shortcuts_and_current_values() {
        let all = entries(CURRENT);
        let find = |id: &str| all.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(find("terminal-larger").detail, "Ctrl+=  120%");
        assert_eq!(find("theme:1").detail, "current");
        assert_eq!(find("theme:0").detail, "");
        assert_eq!(find("new-workspace").detail, "Ctrl+Shift+N");
    }

    #[test]
    fn typing_narrows_by_words_and_initials() {
        let titles = |query: &str| matching(entries(CURRENT), query).into_iter().map(|entry| entry.title).collect::<Vec<_>>();
        assert_eq!(titles("graph"), ["Theme: Graphite"]);
        assert_eq!(titles("text smaller"), ["Terminal Text Smaller", "Interface Text Smaller"]);
        assert_eq!(titles("tts")[0], "Terminal Text Smaller");
        assert!(titles("nothing like this").is_empty());
        assert_eq!(titles("").len(), entries(CURRENT).len());
    }

    #[test]
    fn text_size_steps_stay_in_range() {
        assert_eq!(step(100, true), 110);
        assert_eq!(step(50, false), 50);
        assert_eq!(step(200, true), 200);
    }
}
