pub fn terminal_safe_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => safe.push('\n'),
            '\t' => safe.push_str("    "),
            character if character.is_control() => {}
            character => safe.push(character),
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::terminal_safe_text;

    #[test]
    fn strips_terminal_controls_and_preserves_printable_unicode_and_newlines() {
        assert_eq!(
            terminal_safe_text("中文\x1b]0;title\x07\nline\tend"),
            "中文]0;title\nline    end"
        );
    }
}
