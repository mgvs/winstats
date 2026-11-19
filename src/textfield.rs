//! A one-line text field drawn inside a popup (no child window): the text, a caret and a
//! select-all state, edited by WM_CHAR / WM_KEYDOWN. Enter and Esc end the edit.

/// What a key did to the field.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyResult {
    /// the text or the caret changed: redraw
    Changed,
    /// Enter: the text as it stands
    Commit(String),
    /// Esc: leave the old value alone
    Cancel,
    /// not a key the field handles
    Ignored,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextField {
    pub text: String,
    /// caret position in chars
    pub caret: usize,
    /// everything selected (as on opening): typing replaces it
    pub all: bool,
    pub max_len: usize,
}

const VK_BACK: u32 = 0x08;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_RIGHT: u32 = 0x27;
const VK_DELETE: u32 = 0x2E;

impl TextField {
    /// Open with `text`, all of it selected.
    pub fn new(text: &str, max_len: usize) -> Self {
        Self { text: text.to_string(), caret: text.chars().count(), all: !text.is_empty(), max_len }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map_or(self.text.len(), |(b, _)| b)
    }

    fn clear_if_all(&mut self) {
        if self.all {
            self.text.clear();
            self.caret = 0;
            self.all = false;
        }
    }

    /// A typed character (WM_CHAR); control characters are ignored.
    pub fn insert(&mut self, c: char) -> KeyResult {
        if c.is_control() {
            return KeyResult::Ignored;
        }
        self.clear_if_all();
        if self.len() >= self.max_len {
            return KeyResult::Ignored;
        }
        let b = self.byte(self.caret);
        self.text.insert(b, c);
        self.caret += 1;
        KeyResult::Changed
    }

    /// Pasted text: one line, cut to the room left.
    pub fn paste(&mut self, s: &str) -> KeyResult {
        let line: String = s.lines().next().unwrap_or("").trim().chars().filter(|c| !c.is_control()).collect();
        if line.is_empty() {
            return KeyResult::Ignored;
        }
        self.clear_if_all();
        for c in line.chars() {
            if self.len() >= self.max_len {
                break;
            }
            let b = self.byte(self.caret);
            self.text.insert(b, c);
            self.caret += 1;
        }
        KeyResult::Changed
    }

    /// A key (WM_KEYDOWN); `ctrl` held for Ctrl+A.
    pub fn key(&mut self, vk: u32, ctrl: bool) -> KeyResult {
        match vk {
            VK_RETURN => KeyResult::Commit(self.text.trim().to_string()),
            VK_ESCAPE => KeyResult::Cancel,
            0x41 if ctrl => {
                self.all = !self.text.is_empty();
                self.caret = self.len();
                KeyResult::Changed
            }
            VK_BACK => {
                if self.all {
                    self.clear_if_all();
                } else if self.caret > 0 {
                    let b = self.byte(self.caret - 1);
                    self.text.remove(b);
                    self.caret -= 1;
                }
                KeyResult::Changed
            }
            VK_DELETE => {
                if self.all {
                    self.clear_if_all();
                } else if self.caret < self.len() {
                    let b = self.byte(self.caret);
                    self.text.remove(b);
                }
                KeyResult::Changed
            }
            VK_LEFT => {
                if self.all {
                    self.all = false;
                    self.caret = 0;
                } else {
                    self.caret = self.caret.saturating_sub(1);
                }
                KeyResult::Changed
            }
            VK_RIGHT => {
                self.all = false;
                self.caret = (self.caret + 1).min(self.len());
                KeyResult::Changed
            }
            VK_HOME => {
                self.all = false;
                self.caret = 0;
                KeyResult::Changed
            }
            VK_END => {
                self.all = false;
                self.caret = self.len();
                KeyResult::Changed
            }
            _ => KeyResult::Ignored,
        }
    }

    /// The text before the caret (for placing it).
    pub fn before_caret(&self) -> &str {
        &self.text[..self.byte(self.caret)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_the_selected_old_value() {
        let mut f = TextField::new("1.1.1.1", 63);
        assert!(f.all);
        for c in "8.8.8.8".chars() {
            f.insert(c);
        }
        assert_eq!(f.text, "8.8.8.8");
        assert_eq!(f.key(VK_RETURN, false), KeyResult::Commit("8.8.8.8".into()));
    }

    #[test]
    fn editing_keys_move_and_delete() {
        let mut f = TextField::new("1.1.1.1", 63);
        f.key(VK_END, false);
        f.key(VK_BACK, false);
        f.insert('2');
        assert_eq!(f.text, "1.1.1.2");
        f.key(VK_HOME, false);
        f.key(VK_DELETE, false);
        f.insert('9');
        assert_eq!(f.text, "9.1.1.2");
        assert_eq!(f.before_caret(), "9");
        f.key(VK_RIGHT, false);
        f.key(VK_LEFT, false);
        assert_eq!(f.caret, 1);
    }

    #[test]
    fn select_all_then_backspace_empties() {
        let mut f = TextField::new("router.local", 63);
        f.key(VK_HOME, false);
        f.key(0x41, true);
        f.key(VK_BACK, false);
        assert_eq!(f.text, "");
        assert_eq!(f.key(VK_RETURN, false), KeyResult::Commit(String::new()));
    }

    #[test]
    fn paste_takes_one_trimmed_line_and_respects_the_limit() {
        let mut f = TextField::new("x", 8);
        f.paste("  one.one.one.one\nsecond line");
        assert_eq!(f.text, "one.one.");
        assert_eq!(f.key(VK_ESCAPE, false), KeyResult::Cancel);
        assert_eq!(f.insert('\u{8}'), KeyResult::Ignored);
    }
}
