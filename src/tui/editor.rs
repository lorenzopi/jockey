//! Multi-line prompt editor. `cursor` is a byte offset into `text`.

use unicode_width::UnicodeWidthChar;

#[derive(Default, Debug)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}

impl Editor {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, s: &str) {
        let s = s
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self, i: usize) -> usize {
        self.text[..i].char_indices().last().map_or(0, |(j, _)| j)
    }

    fn next_boundary(&self, i: usize) -> usize {
        self.text[i..]
            .chars()
            .next()
            .map_or(i, |c| i + c.len_utf8())
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev_boundary(self.cursor);
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next_boundary(self.cursor);
            self.text.replace_range(self.cursor..n, "");
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
    }

    fn word_start_before(&self, i: usize) -> usize {
        let t = &self.text[..i];
        let trimmed = t.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .rfind(|c: char| !c.is_alphanumeric())
            .map_or(0, |j| j + trimmed[j..].chars().next().unwrap().len_utf8())
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
    }

    pub fn word_right(&mut self) {
        let t = &self.text[self.cursor..];
        let skip = t.find(|c: char| c.is_alphanumeric()).unwrap_or(t.len());
        let rest = &t[skip..];
        let end = rest
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(rest.len());
        self.cursor += skip + end;
    }

    pub fn delete_word_back(&mut self) {
        let s = self.word_start_before(self.cursor);
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        let e = if e == self.cursor && e < self.text.len() {
            e + 1
        } else {
            e
        };
        self.text.replace_range(self.cursor..e, "");
    }

    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    /// Moves up one logical line. Returns false when already on the first line.
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = advance(&self.text, prev_start, start - 1, col);
        true
    }

    /// Moves down one logical line. Returns false when already on the last line.
    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let col = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |i| next_start + i);
        self.cursor = advance(&self.text, next_start, next_end, col);
        true
    }

    /// Hard-wrapped rows for display and the cursor's (row, col) within them.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut cur = (0, 0);
        for (i, ch) in self.text.char_indices() {
            if i == self.cursor {
                cur = (rows.len() - 1, col);
            }
            if ch == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(ch);
            col += w;
        }
        if self.cursor >= self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cur = (rows.len() - 1, col);
        }
        (rows, cur)
    }
}

/// Byte offset `col` chars into [from, to), clamped to `to`.
fn advance(text: &str, from: usize, to: usize, col: usize) -> usize {
    text[from..to]
        .char_indices()
        .nth(col)
        .map_or(to, |(i, _)| from + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_navigation() {
        let mut e = Editor::default();
        e.insert("hello wörld");
        e.word_left();
        assert_eq!(&e.text[e.cursor..], "wörld");
        e.delete_word_back();
        assert_eq!(e.text, "wörld");
        e.end();
        e.insert("\nsecond");
        assert!(e.up());
        assert!(!e.up());
        assert!(e.down());
        e.home();
        e.kill_to_end();
        assert_eq!(e.text, "wörld\n");
        e.backspace();
        e.backspace();
        assert_eq!(e.text, "wörl");
    }

    #[test]
    fn layout_wraps_and_tracks_cursor() {
        let mut e = Editor::default();
        e.insert("abcdef\ngh");
        let (rows, cur) = e.layout(4);
        assert_eq!(rows, vec!["abcd", "ef", "gh"]);
        assert_eq!(cur, (2, 2));
    }
}
