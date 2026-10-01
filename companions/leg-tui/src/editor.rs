use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default)]
pub(super) struct ComposerEditor {
    text: String,
    cursor: usize,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
}

#[derive(Debug)]
struct Edit {
    start: usize,
    removed: String,
    inserted: String,
    cursor_before: usize,
    cursor_after: usize,
}

impl ComposerEditor {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.undo.clear();
        self.redo.clear();
    }

    pub(super) fn clear(&mut self) {
        self.set_text(String::new());
    }

    pub(super) fn insert(&mut self, value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        self.replace(self.cursor..self.cursor, value);
        true
    }

    pub(super) fn backspace(&mut self) -> bool {
        let Some((start, _)) = self.text[..self.cursor].grapheme_indices(true).last() else {
            return false;
        };
        self.replace(start..self.cursor, "");
        true
    }

    pub(super) fn delete(&mut self) -> bool {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return false;
        };
        let end = self.cursor + grapheme.len();
        self.replace(self.cursor..end, "");
        true
    }

    pub(super) fn move_left(&mut self) {
        if let Some((start, _)) = self.text[..self.cursor].grapheme_indices(true).last() {
            self.cursor = start;
        }
    }

    pub(super) fn move_right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |newline| self.cursor + newline);
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        self.apply_edit(&edit, false);
        self.redo.push(edit);
        true
    }

    pub(super) fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        self.apply_edit(&edit, true);
        self.undo.push(edit);
        true
    }

    pub(super) fn cursor_position(&self, width: usize) -> (usize, usize) {
        let width = width.max(1);
        let mut row = 0;
        let mut column = 0;
        for grapheme in self.text[..self.cursor].graphemes(true) {
            if grapheme == "\n" {
                row += 1;
                column = 0;
                continue;
            }
            let grapheme_width = UnicodeWidthStr::width(grapheme).min(width);
            if column + grapheme_width > width {
                row += 1;
                column = 0;
            }
            column += grapheme_width;
            if column == width {
                row += 1;
                column = 0;
            }
        }
        (row, column)
    }

    fn replace(&mut self, range: Range<usize>, inserted: &str) {
        let removed = self.text[range.clone()].to_string();
        if removed.is_empty() && inserted.is_empty() {
            return;
        }
        let cursor_before = self.cursor;
        let start = range.start;
        self.text.replace_range(range, inserted);
        self.cursor = start + inserted.len();
        let edit = Edit {
            start,
            removed,
            inserted: inserted.to_string(),
            cursor_before,
            cursor_after: self.cursor,
        };
        self.undo.push(edit);
        self.redo.clear();
    }

    fn apply_edit(&mut self, edit: &Edit, forward: bool) {
        if forward {
            let end = edit.start + edit.removed.len();
            self.text
                .replace_range(edit.start..end, edit.inserted.as_str());
            self.cursor = edit.cursor_after;
        } else {
            let end = edit.start + edit.inserted.len();
            self.text
                .replace_range(edit.start..end, edit.removed.as_str());
            self.cursor = edit.cursor_before;
        }
    }
}

pub(super) fn normalize_paste(input: &str) -> String {
    input
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|character| *character == '\n' || !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ComposerEditor, normalize_paste};

    #[test]
    fn movement_and_deletion_keep_grapheme_boundaries() {
        let mut editor = ComposerEditor::default();
        editor.insert("a\u{301}中👩‍👩‍👧‍👦z");

        editor.move_left();
        assert_eq!(editor.text(), "a\u{301}中👩‍👩‍👧‍👦z");
        editor.backspace();
        assert_eq!(editor.text(), "a\u{301}中z");
        editor.move_left();
        editor.delete();
        assert_eq!(editor.text(), "a\u{301}z");
        assert!(editor.text().is_char_boundary(editor.cursor()));
    }

    #[test]
    fn home_and_end_stop_at_current_line_boundaries() {
        let mut editor = ComposerEditor::default();
        editor.insert("first\n第二行🙂\nlast");
        editor.move_home();
        assert_eq!(&editor.text()[editor.cursor()..], "last");
        editor.move_left();
        editor.move_home();
        assert_eq!(&editor.text()[editor.cursor()..], "第二行🙂\nlast");
        editor.move_end();
        assert_eq!(&editor.text()[editor.cursor()..], "\nlast");
    }

    #[test]
    fn undo_and_redo_restore_text_and_edit_cursor() {
        let mut editor = ComposerEditor::default();
        editor.insert("base");
        editor.insert(&normalize_paste(" paste\r\n第二行🙂\u{13}"));
        let pasted_cursor = editor.cursor();
        editor.move_left();

        assert!(editor.undo());
        assert_eq!(editor.text(), "base");
        assert_eq!(editor.cursor(), 4);
        assert!(editor.redo());
        assert_eq!(editor.text(), "base paste\n第二行🙂");
        assert_eq!(editor.cursor(), pasted_cursor);
        assert!(!editor.redo());
    }

    #[test]
    fn paste_normalizes_newlines_and_discards_controls() {
        assert_eq!(
            normalize_paste("一\r\n二\r三\n四\u{13}\u{1b}\u{7f}\0"),
            "一\n二\n三\n四"
        );
    }

    #[test]
    fn cursor_position_uses_unicode_display_width() {
        let mut editor = ComposerEditor::default();
        editor.insert("中a");
        assert_eq!(editor.cursor_position(8), (0, 3));
        editor.insert("bcd");
        assert_eq!(editor.cursor_position(4), (1, 2));
    }
}
