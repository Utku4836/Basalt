//! The command being typed at the shell's prompt, read from the screen. The session's
//! shell starts every prompt with its angle, so a cursor on a line that starts with it
//! is at the command line; full-screen programs and scrolled views never are.

use basalt_core::terminal::Screen;

/// The glyph the session's prompt starts with (see `shell.rs`).
pub const PROMPT: char = '\u{F105}';
const ALT_SCREEN: u32 = 1 << 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub row: usize,
    /// The column the command starts at, after the prompt.
    pub start: usize,
    pub text: String,
    /// Characters of `text` before the cursor.
    pub cursor: usize,
}

impl CommandLine {
    /// Whether the cursor sits after the last typed character.
    pub fn at_end(&self) -> bool {
        self.cursor >= self.text.chars().count()
    }
}

pub fn read(screen: &Screen) -> Option<CommandLine> {
    if screen.display_offset != 0 || screen.mode & ALT_SCREEN != 0 || screen.cursor_line < 0 {
        return None;
    }
    let row = screen.cursor_line as usize;
    if row >= screen.rows {
        return None;
    }
    let cells = screen.row(row);
    if cells.len() < 3 || cells[0].c != PROMPT || cells[1].c != ' ' {
        return None;
    }
    let start = 2;
    // Spacer cells after wide characters hold no character of their own.
    let mut text: String = cells[start..].iter().filter(|cell| cell.flags & 64 == 0).map(|cell| cell.c).collect();
    text.truncate(text.trim_end().len());
    let cursor = cells[start..screen.cursor_column.max(start).min(cells.len())]
        .iter()
        .filter(|cell| cell.flags & 64 == 0)
        .count();
    Some(CommandLine { row, start, text, cursor })
}

#[cfg(test)]
mod tests {
    use super::*;
    use basalt_core::terminal::{TerminalDecoder, TerminalSize};

    fn screen(bytes: &str) -> Screen {
        let mut decoder = TerminalDecoder::new(TerminalSize::new(40, 4, 8, 16).unwrap(), 10).unwrap();
        decoder.feed(bytes.as_bytes());
        let mut screen = Screen::default();
        decoder.model().read_screen(&mut screen);
        screen
    }

    #[test]
    fn reads_the_typed_command_and_the_cursor() {
        let line = read(&screen("output\r\n\x1b[1m\u{F105}\x1b[0m git sta")).unwrap();
        assert_eq!((line.row, line.start, line.text.as_str(), line.cursor), (1, 2, "git sta", 7));
        assert!(line.at_end());
        let moved = read(&screen("\u{F105} git status\x1b[3D")).unwrap();
        assert_eq!(moved.cursor, 7);
        assert!(!moved.at_end());
    }

    #[test]
    fn output_and_full_screen_programs_are_not_a_command_line() {
        assert!(read(&screen("\u{F105} ls\r\nfile.txt")).is_none());
        assert!(read(&screen("\x1b[?1049h\u{F105} inside an editor")).is_none());
        assert_eq!(read(&screen("\u{F105} ")).unwrap().text, "");
    }
}
