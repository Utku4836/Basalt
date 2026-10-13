//! Visible-grid text selection and safe terminal paste encoding.
use basalt_core::terminal::TerminalSnapshot;
pub fn selected_text(snapshot: &TerminalSnapshot, a: (usize, usize), b: (usize, usize)) -> String {
    let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    };
    let cells = snapshot
        .cells
        .iter()
        .map(|cell| {
            (
                (cell.line + snapshot.display_offset as i32, cell.column),
                cell,
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut result = String::new();
    for row in start.1..=end.1.min(snapshot.rows.saturating_sub(1)) {
        let left = if row == start.1 { start.0 } else { 0 };
        let right = if row == end.1 {
            end.0
        } else {
            snapshot.columns.saturating_sub(1)
        };
        let mut line = String::new();
        let mut wrapped = false;
        for col in left..=right.min(snapshot.columns.saturating_sub(1)) {
            if let Some(cell) = cells.get(&(row as i32, col)) {
                if cell.flags & (64 | 1024) == 0 {
                    line.push_str(&cell.text);
                }
                if col + 1 == snapshot.columns {
                    wrapped = cell.flags & 16 != 0;
                }
            } else {
                line.push(' ');
            }
        }
        if !wrapped {
            line.truncate(line.trim_end_matches(' ').len());
        }
        result.push_str(&line);
        if row < end.1 && !wrapped {
            result.push('\n');
        }
    }
    result
}
pub fn paste_bytes(text: &str, mode: u32) -> Vec<u8> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let clean: String = text
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect();
    if mode & 16 != 0 {
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        clean.replace('\n', "\r").into_bytes()
    }
}

/// Native tests use an isolated in-memory clipboard, preserving the user's clipboard.
pub struct Clipboard {
    memory: Option<String>,
}
impl Clipboard {
    pub fn new(isolated: bool) -> Self {
        Self {
            memory: isolated.then(String::new),
        }
    }
    pub fn read(&self) -> Result<String, String> {
        if let Some(text) = &self.memory {
            Ok(text.clone())
        } else {
            arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.get_text())
                .map_err(|e| format!("Clipboard unavailable: {e}"))
        }
    }
    pub fn write(&mut self, text: String) -> Result<(), String> {
        if let Some(memory) = &mut self.memory {
            *memory = text;
            Ok(())
        } else {
            arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(text))
                .map_err(|e| format!("Clipboard unavailable: {e}"))
        }
    }
}

pub fn key_text(text: &str, control: bool) -> String {
    if control && text == "\u{3}" {
        "c".into()
    } else if control && text == "\u{16}" {
        "v".into()
    } else {
        text.into()
    }
}
