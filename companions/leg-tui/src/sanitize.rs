#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

/// Incremental terminal text filter. Escape sequences are discarded as a
/// whole, including when their bytes arrive in separate stream deltas.
#[derive(Clone, Debug, Default)]
pub struct TerminalSanitizer {
    state: State,
}

impl TerminalSanitizer {
    pub fn push(&mut self, text: &str) -> String {
        let mut safe = String::with_capacity(text.len());
        for character in text.chars() {
            self.accept(character, &mut safe);
        }
        safe
    }

    /// Discard an unfinished sequence at the end of its untrusted input.
    pub fn finish(&mut self) {
        self.state = State::Ground;
    }

    fn accept(&mut self, character: char, safe: &mut String) {
        match self.state {
            State::Ground => match character {
                '\u{001b}' => self.state = State::Escape,
                '\u{009b}' => self.state = State::Csi,
                '\u{009d}' => self.state = State::Osc,
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                    self.state = State::ControlString;
                }
                '\n' => safe.push('\n'),
                '\t' => safe.push_str("    "),
                character if character.is_control() => {}
                character => safe.push(character),
            },
            State::Escape => match character {
                '[' => self.state = State::Csi,
                ']' => self.state = State::Osc,
                'P' | 'X' | '^' | '_' => self.state = State::ControlString,
                '\u{001b}' => self.state = State::Escape,
                character if (' '..='/').contains(&character) => {}
                character if ('@'..='_').contains(&character) => self.state = State::Ground,
                character if character.is_control() => self.state = State::Ground,
                _ => self.state = State::Ground,
            },
            State::Csi => match character {
                '\u{001b}' => self.state = State::Escape,
                character if ('@'..='~').contains(&character) => self.state = State::Ground,
                _ => {}
            },
            State::Osc => match character {
                '\u{0007}' | '\u{009c}' => self.state = State::Ground,
                '\u{001b}' => self.state = State::OscEscape,
                _ => {}
            },
            State::OscEscape => match character {
                '\\' => self.state = State::Ground,
                '\u{001b}' => self.state = State::OscEscape,
                '\u{0007}' | '\u{009c}' => self.state = State::Ground,
                _ => self.state = State::Osc,
            },
            State::ControlString => match character {
                '\u{009c}' => self.state = State::Ground,
                '\u{001b}' => self.state = State::ControlStringEscape,
                _ => {}
            },
            State::ControlStringEscape => match character {
                '\\' | '\u{009c}' => self.state = State::Ground,
                '\u{001b}' => self.state = State::ControlStringEscape,
                _ => self.state = State::ControlString,
            },
        }
    }
}

pub fn terminal_safe_text(text: &str) -> String {
    let mut sanitizer = TerminalSanitizer::default();
    let mut safe = sanitizer.push(text);
    sanitizer.finish();
    safe.shrink_to_fit();
    safe
}

#[cfg(test)]
mod tests {
    use super::{TerminalSanitizer, terminal_safe_text};

    #[test]
    fn removes_complete_ansi_csi_osc_and_control_strings() {
        assert_eq!(
            terminal_safe_text(
                "前\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}ok\u{1b}Psecret\u{1b}\\尾\nline\tend"
            ),
            "前red ok尾\nline    end"
        );
    }

    #[test]
    fn removes_sequences_split_across_deltas_without_leaking_fragments() {
        let mut sanitizer = TerminalSanitizer::default();
        assert_eq!(sanitizer.push("start\u{1b}["), "start");
        assert_eq!(sanitizer.push("31mred\u{1b}]0;ti"), "red");
        assert_eq!(sanitizer.push("tle\u{7}中文"), "中文");
        sanitizer.finish();
        assert_eq!(sanitizer.push("\u{1b}[31;"), "");
        sanitizer.finish();
    }

    #[test]
    fn preserves_printable_unicode_and_literal_newlines() {
        assert_eq!(terminal_safe_text("中文\n👩‍👩‍👧‍👦 e\u{301}"), "中文\n👩‍👩‍👧‍👦 e\u{301}");
    }
}
