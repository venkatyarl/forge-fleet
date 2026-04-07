//! Input handling — text editing, history, slash command autocomplete.

/// Input state for the prompt editor.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub text: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    pub history_draft: String,
    pub suggestions: Vec<String>,
    pub suggestion_index: Option<usize>,
}

impl InputState {
    pub fn new() -> Self { Self::default() }

    /// Insert a character at cursor position.
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.clear_suggestions();
    }

    /// Delete character before cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.drain(self.cursor..next);
        }
    }

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Move cursor to start.
    pub fn home(&mut self) { self.cursor = 0; }

    /// Move cursor to end.
    pub fn end(&mut self) { self.cursor = self.text.len(); }

    /// Submit the current text (returns it and clears input).
    pub fn submit(&mut self) -> String {
        let text = self.text.clone();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.text.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.clear_suggestions();
        text
    }

    /// Navigate history up.
    pub fn history_up(&mut self) {
        if self.history.is_empty() { return; }

        match self.history_pos {
            None => {
                self.history_draft = self.text.clone();
                self.history_pos = Some(self.history.len() - 1);
            }
            Some(pos) if pos > 0 => {
                self.history_pos = Some(pos - 1);
            }
            _ => return,
        }

        if let Some(pos) = self.history_pos {
            self.text = self.history[pos].clone();
            self.cursor = self.text.len();
        }
    }

    /// Navigate history down.
    pub fn history_down(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                self.history_pos = Some(pos + 1);
                self.text = self.history[pos + 1].clone();
                self.cursor = self.text.len();
            }
            Some(_) => {
                self.history_pos = None;
                self.text = self.history_draft.clone();
                self.cursor = self.text.len();
            }
            None => {}
        }
    }

    /// Compute slash command suggestions.
    pub fn compute_suggestions(&mut self, commands: &[(&str, &str)]) {
        self.suggestions.clear();
        self.suggestion_index = None;

        if !self.text.starts_with('/') { return; }

        let prefix = self.text[1..].to_ascii_lowercase();
        for (name, desc) in commands {
            if name.starts_with(&prefix) || prefix.is_empty() {
                self.suggestions.push(format!("/{name} — {desc}"));
            }
        }
    }

    fn clear_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestion_index = None;
    }

    /// Accept the current suggestion.
    pub fn accept_suggestion(&mut self) {
        if let Some(idx) = self.suggestion_index {
            if let Some(suggestion) = self.suggestions.get(idx) {
                if let Some(cmd) = suggestion.split(' ').next() {
                    self.text = format!("{cmd} ");
                    self.cursor = self.text.len();
                }
            }
        }
        self.clear_suggestions();
    }

    /// Cycle through suggestions.
    pub fn next_suggestion(&mut self) {
        if self.suggestions.is_empty() { return; }
        self.suggestion_index = Some(match self.suggestion_index {
            None => 0,
            Some(i) => (i + 1) % self.suggestions.len(),
        });
    }
}
