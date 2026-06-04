#[allow(unused_imports)]
use crate::sync_util::LockExt;
use std::path::{Component, PathBuf};
use std::sync::{Arc, Mutex};

/// Owned snapshot of a picker's display state, handed to the renderer so its
/// candidate list is painted through the ratatui scene — visible on `/dev/tty`,
/// diff-safe, and inheriting the theme background. Previously the pickers wrote
/// raw cursor-positioned escapes to `std::io::stdout()`, which `TerminalGuard`
/// redirects to the log, so the overlay never reached the screen [dirge-92em].
#[derive(Clone, Default)]
pub struct PickerOverlay {
    /// Header line (the `ListPicker` prompt); `None` for the file picker.
    pub title: Option<String>,
    /// Candidate rows as pre-formatted display strings.
    pub rows: Vec<String>,
    /// Index of the highlighted row into `rows`.
    pub selected: usize,
    /// Shown when `rows` is empty (e.g. `"no matches"`).
    pub empty_hint: Option<String>,
}

pub struct ListPicker {
    pub active: bool,
    pub items: Vec<String>,
    pub selected: usize,
    prompt: String,
}

impl ListPicker {
    pub fn new() -> Self {
        ListPicker {
            active: false,
            items: Vec::new(),
            selected: 0,
            prompt: String::new(),
        }
    }

    pub fn activate(&mut self, prompt: &str, items: Vec<String>) {
        self.active = true;
        self.prompt = prompt.to_string();
        self.items = items;
        self.selected = 0;
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Option<usize> {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Enter => {
                if self.items.is_empty() {
                    self.active = false;
                    None
                } else {
                    let result = Some(self.selected);
                    self.active = false;
                    result
                }
            }
            KeyCode::Esc => {
                self.active = false;
                None
            }
            _ => None,
        }
    }

    /// Snapshot for the renderer to paint through the ratatui scene.
    pub fn overlay(&self) -> PickerOverlay {
        PickerOverlay {
            title: (!self.prompt.is_empty()).then(|| self.prompt.clone()),
            rows: self.items.clone(),
            selected: self.selected,
            empty_hint: None,
        }
    }
}

pub struct FilePicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
    file_cache: Arc<Mutex<Vec<PathBuf>>>,
}

impl FilePicker {
    pub fn new() -> Self {
        FilePicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            file_cache: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.load_files();
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    fn load_files(&mut self) {
        let files = walk_files(".");
        *self.file_cache.lock_ignore_poison() = files;
    }

    pub fn char_input(&mut self, c: char) {
        // Convert char-index cursor to byte index for String::insert
        let byte_pos = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert(byte_pos, c);
        self.cursor += 1;
        self.filter();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 && !self.query.is_empty() {
            self.cursor -= 1;
            // Convert char-index cursor to byte index for String::remove
            let byte_pos = self
                .query
                .char_indices()
                .nth(self.cursor)
                .map(|(i, _)| i)
                .unwrap_or(self.query.len());
            self.query.remove(byte_pos);
            self.filter();
        }
    }

    fn filter(&mut self) {
        let cache = self.file_cache.lock_ignore_poison();
        if cache.is_empty() {
            self.matches.clear();
            return;
        }
        let query_lower = self.query.to_lowercase();
        self.matches = cache
            .iter()
            .filter(|p| {
                let lower = p.to_string_lossy().to_lowercase();
                lower.contains(&query_lower)
            })
            .take(50)
            .cloned()
            .collect();
        self.selected = 0;
    }

    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = if self.selected == 0 {
                self.matches.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn selected_path(&self) -> Option<&PathBuf> {
        self.matches.get(self.selected)
    }

    #[cfg(test)]
    pub fn test_set_cache(&mut self, files: Vec<PathBuf>) {
        *self.file_cache.lock_ignore_poison() = files;
    }

    /// Snapshot for the renderer to paint above the input box through the
    /// ratatui scene. Rows are the matched paths; an empty match set surfaces
    /// the `"no matches"` hint.
    pub fn overlay(&self) -> PickerOverlay {
        PickerOverlay {
            title: None,
            rows: self
                .matches
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            selected: self.selected,
            empty_hint: Some("no matches".to_string()),
        }
    }
}

fn walk_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(8))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .components()
            .any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let rel = rel.trim_start_matches('/').to_string();
        files.push(PathBuf::from(rel));
        if files.len() >= 200 {
            break;
        }
    }
    files
}
