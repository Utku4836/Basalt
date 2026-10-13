// This file has local integration changes; see vendor/CHANGES.md.
//! Progress reports (`OSC 9;4;state;value`), which the parser does not keep. Some
//! programs report whether they are busy only this way.

const START: &[u8] = b"\x1b]9;4";
/// A report longer than this is not one.
const LONGEST: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct Progress {
    /// The last report, from `4` on (`4;1;50`), or empty before any.
    value: String,
    /// The start of a report cut off at the end of the last read.
    partial: Vec<u8>,
}

impl Progress {
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Look for reports in the next bytes of output.
    pub fn scan(&mut self, bytes: &[u8]) {
        if self.partial.is_empty() {
            self.scan_from(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(bytes);
            self.scan_from(&joined);
        }
    }

    fn scan_from(&mut self, data: &[u8]) {
        let mut index = 0;
        while let Some(found) = data[index..].iter().position(|byte| *byte == 0x1b) {
            let start = index + found;
            let rest = &data[start..];
            if rest.len() < START.len() {
                if START.starts_with(rest) {
                    self.partial = rest.to_vec();
                }
                return;
            }
            if !rest.starts_with(START) {
                index = start + 1;
                continue;
            }
            let body = &rest[START.len() - 1..];
            let end = body.iter().enumerate().position(|(at, byte)| {
                *byte == 0x07 || (*byte == 0x1b && body.get(at + 1) == Some(&b'\\'))
            });
            match end {
                Some(end) => {
                    if let Ok(text) = std::str::from_utf8(&body[..end]) {
                        self.value = text.to_owned();
                    }
                    index = start + START.len() - 1 + end;
                },
                None => {
                    if rest.len() <= LONGEST {
                        self.partial = rest.to_vec();
                    }
                    return;
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Progress;

    #[test]
    fn keeps_the_last_report() {
        let mut progress = Progress::default();
        progress.scan(b"text\x1b]9;4;1;-1\x07more\x1b]9;4;3\x1b\\end");
        assert_eq!(progress.value(), "4;3");
    }

    #[test]
    fn joins_a_report_split_across_reads() {
        let mut progress = Progress::default();
        progress.scan(b"abc\x1b]9");
        progress.scan(b";4;0;0");
        assert_eq!(progress.value(), "");
        progress.scan(b"\x07");
        assert_eq!(progress.value(), "4;0;0");
    }

    #[test]
    fn ignores_other_sequences() {
        let mut progress = Progress::default();
        progress.scan(b"\x1b]0;title\x07\x1b]9;notify\x07\x1b[1m");
        assert_eq!(progress.value(), "");
    }
}
