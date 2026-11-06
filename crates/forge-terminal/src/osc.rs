//! The OSC 133 scanner: exact command marks from the shell's own integration.
//!
//! The shell writes `OSC 133 ; A` at the prompt start, `; B` at the prompt
//! end, `; C` before a command runs and `; D [;<exit>]` when it finishes. The
//! scanner reads those sequences out of the byte stream **before** the bytes
//! reach `Term`, records the marks, and leaves the bytes untouched - the
//! client sees exactly what the shell wrote.
//!
//! A sequence split across two reads is carried in the scanner's own state,
//! so a mark is never lost to a read boundary.

/// One command mark the shell emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The prompt is about to be drawn.
    PromptStart,
    /// The prompt is done; the person may type.
    PromptEnd,
    /// A command started running.
    CommandStart,
    /// A command finished, with the exit code when the shell carried one.
    CommandEnd(Option<i32>),
}

/// The marks seen so far, and the state they imply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// Whether any OSC 133 mark has been seen. `false` means the shell has no
    /// integration and the classifier must fall back to the heuristic.
    pub seen: bool,
    /// Whether a command is running: a `C` with no `D` after it.
    pub running: bool,
    /// The exit code the last `D` carried, when it carried one.
    pub last_exit: Option<i32>,
    /// The last mark seen.
    pub last: Option<Mark>,
}

/// The scanner's position in the byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scan {
    Ground,
    Escape,
    Osc,
    OscEscape,
}

/// The most of one OSC payload the scanner keeps. A clipboard write can be
/// megabytes; the scanner only needs the `133;` prefix, so a payload past
/// this is discarded rather than held.
const MAX_PAYLOAD: usize = 4096;

/// The OSC 133 scanner.
#[derive(Debug)]
pub struct Osc133 {
    scan: Scan,
    payload: Vec<u8>,
    discard: bool,
    marks: Marks,
}

impl Default for Osc133 {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc133 {
    pub fn new() -> Self {
        Self { scan: Scan::Ground, payload: Vec::new(), discard: false, marks: Marks::default() }
    }

    /// The marks seen so far.
    pub fn marks(&self) -> Marks {
        self.marks
    }

    /// Scan one read's bytes, recording any marks. The bytes are not changed.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    fn step(&mut self, byte: u8) {
        match self.scan {
            Scan::Ground => {
                if byte == 0x1b {
                    self.scan = Scan::Escape;
                }
            }
            Scan::Escape => match byte {
                b']' => {
                    self.scan = Scan::Osc;
                    self.payload.clear();
                    self.discard = false;
                }
                0x1b => {}
                _ => self.scan = Scan::Ground,
            },
            Scan::Osc => match byte {
                0x07 => {
                    self.finish();
                    self.scan = Scan::Ground;
                }
                0x1b => self.scan = Scan::OscEscape,
                _ => self.push(byte),
            },
            Scan::OscEscape => match byte {
                b'\\' => {
                    self.finish();
                    self.scan = Scan::Ground;
                }
                0x1b => {}
                _ => {
                    // The ESC ended the OSC without a string terminator; the
                    // byte starts whatever comes next.
                    self.finish();
                    self.scan = Scan::Ground;
                    self.step(byte);
                }
            },
        }
    }

    fn push(&mut self, byte: u8) {
        if self.discard {
            return;
        }
        if self.payload.len() >= MAX_PAYLOAD {
            self.discard = true;
            self.payload.clear();
            return;
        }
        self.payload.push(byte);
    }

    fn finish(&mut self) {
        if self.discard {
            self.discard = false;
            self.payload.clear();
            return;
        }
        let payload = std::mem::take(&mut self.payload);
        let Some(rest) = payload.strip_prefix(b"133;") else { return };
        let mark = match rest {
            b"A" => Mark::PromptStart,
            b"B" => Mark::PromptEnd,
            b"C" => Mark::CommandStart,
            b"D" => Mark::CommandEnd(None),
            other => {
                let Some(code) = other.strip_prefix(b"D;") else { return };
                let Ok(text) = std::str::from_utf8(code) else { return };
                let Ok(code) = text.parse::<i32>() else { return };
                Mark::CommandEnd(Some(code))
            }
        };
        self.record(mark);
    }

    fn record(&mut self, mark: Mark) {
        self.marks.seen = true;
        self.marks.last = Some(mark);
        match mark {
            Mark::CommandStart => self.marks.running = true,
            Mark::CommandEnd(code) => {
                self.marks.running = false;
                self.marks.last_exit = code;
            }
            Mark::PromptStart | Mark::PromptEnd => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8]) -> Marks {
        let mut scanner = Osc133::new();
        scanner.feed(bytes);
        scanner.marks()
    }

    /// The four marks arrive in order and leave the state where the shell
    /// says it is: a `C` is a command running, a `D;7` is a failure.
    #[test]
    fn the_four_marks_are_recorded_in_order() {
        let marks =
            scan(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;7\x07");
        assert!(marks.seen, "the shell has integration");
        assert!(!marks.running, "the command finished");
        assert_eq!(marks.last_exit, Some(7), "the failing command's code");
        assert_eq!(marks.last, Some(Mark::CommandEnd(Some(7))));
    }

    /// A `C` with no `D` is a command still running, which is what
    /// `terminal__wait {until: "exit"}` waits on.
    #[test]
    fn a_command_start_with_no_end_is_running() {
        let marks = scan(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(marks.running);
        assert_eq!(marks.last_exit, None);
    }

    /// **A mark split across two reads is not lost.** A read boundary can
    /// fall anywhere, including inside the escape sequence.
    #[test]
    fn a_mark_split_across_reads_is_recorded() {
        let mut scanner = Osc133::new();
        scanner.feed(b"\x1b]13");
        scanner.feed(b"3;D;130");
        scanner.feed(b"\x07");
        let marks = scanner.marks();
        assert_eq!(marks.last_exit, Some(130), "Ctrl-C's code survives the split");
        assert!(!marks.running);
    }

    /// The string terminator may be `ESC \` as well as BEL, and both end the
    /// same mark.
    #[test]
    fn the_esc_backslash_terminator_ends_a_mark() {
        let marks = scan(b"\x1b]133;D;0\x1b\\");
        assert_eq!(marks.last_exit, Some(0));
    }

    /// A shell with no integration leaves `seen` false, which is what makes
    /// the classifier say `heuristic`.
    #[test]
    fn a_stream_with_no_marks_is_not_exact() {
        let marks = scan(b"$ ls\r\nfile\r\n$ ");
        assert!(!marks.seen);
        assert!(!marks.running);
        assert_eq!(marks.last_exit, None);
    }

    /// Another OSC sequence is not a mark, and a malformed `D` is ignored
    /// rather than read as an exit code.
    #[test]
    fn other_osc_sequences_and_malformed_marks_are_ignored() {
        let marks = scan(b"\x1b]0;title\x07\x1b]133;D;notanumber\x07\x1b]133;Z\x07");
        assert!(!marks.seen, "none of those is a mark");
    }

    /// A payload past the cap is discarded rather than held, and the scanner
    /// recovers for the next mark.
    #[test]
    fn an_oversized_payload_is_discarded_and_the_scanner_recovers() {
        let mut scanner = Osc133::new();
        scanner.feed(b"\x1b]52;c;");
        scanner.feed(&vec![b'A'; MAX_PAYLOAD + 10]);
        scanner.feed(b"\x07");
        assert!(!scanner.marks().seen, "the oversized payload is not a mark");
        scanner.feed(b"\x1b]133;D;3\x07");
        assert_eq!(scanner.marks().last_exit, Some(3), "and the next mark is read");
    }
}
