//! A single directory-browser panel: lists subdirectories and files matching a
//! set of extensions, with a movable selection.

use crate::midi;
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// True for the synthetic ".." parent entry.
    pub is_parent: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Duration in seconds for MIDI files, when computed.
    pub duration: Option<f64>,
}

pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub state: ListState,
    /// Lower-case extensions (without dot) that should be shown as files.
    exts: Vec<String>,
    pub show_hidden: bool,
    /// When true, parse MIDI files to report their duration.
    compute_duration: bool,
    /// Cache of parsed MIDI durations, keyed by path.
    duration_cache: HashMap<PathBuf, Option<f64>>,
}

impl Browser {
    pub fn new(dir: PathBuf, exts: &[&str], compute_duration: bool) -> Browser {
        let mut b = Browser {
            dir,
            entries: Vec::new(),
            state: ListState::default(),
            exts: exts.iter().map(|s| s.to_lowercase()).collect(),
            show_hidden: false,
            compute_duration,
            duration_cache: HashMap::new(),
        };
        b.refresh();
        b
    }

    pub fn refresh(&mut self) {
        let mut dirs: Vec<Entry> = Vec::new();
        let mut files: Vec<Entry> = Vec::new();

        if let Ok(rd) = fs::read_dir(&self.dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                let path = ent.path();
                let is_dir = path.is_dir();
                if is_dir {
                    dirs.push(Entry {
                        name,
                        path,
                        is_dir: true,
                        is_parent: false,
                        size: 0,
                        duration: None,
                    });
                } else if self.matches_ext(&path) {
                    let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
                    let duration = if self.compute_duration {
                        *self
                            .duration_cache
                            .entry(path.clone())
                            .or_insert_with(|| midi::parse(&path).map(|i| i.duration_secs))
                    } else {
                        None
                    };
                    files.push(Entry {
                        name,
                        path,
                        is_dir: false,
                        is_parent: false,
                        size,
                        duration,
                    });
                }
            }
        }

        let key = |e: &Entry| e.name.to_lowercase();
        dirs.sort_by_key(key);
        files.sort_by_key(key);

        let mut entries = Vec::with_capacity(dirs.len() + files.len() + 1);
        if self.dir.parent().is_some() {
            entries.push(Entry {
                name: "..".to_string(),
                path: self.dir.parent().unwrap().to_path_buf(),
                is_dir: true,
                is_parent: true,
                size: 0,
                duration: None,
            });
        }
        entries.extend(dirs);
        entries.extend(files);

        self.entries = entries;
        // Keep the selection valid.
        let sel = self.state.selected().unwrap_or(0);
        if self.entries.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(sel.min(self.entries.len() - 1)));
        }
    }

    fn matches_ext(&self, path: &Path) -> bool {
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) => self.exts.iter().any(|x| x == &e.to_lowercase()),
            None => false,
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.state.select(Some(0));
        self.refresh();
    }

    /// Enter the directory at the cursor (or its parent for "..").
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.selected() {
            if e.is_dir {
                let target = e.path.clone();
                self.set_dir(target);
            }
        }
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.dir.parent() {
            let from = self.dir.clone();
            let parent = parent.to_path_buf();
            self.set_dir(parent);
            // Land the cursor on the directory we just came from.
            if let Some(idx) = self.entries.iter().position(|e| !e.is_parent && e.path == from) {
                self.state.select(Some(idx));
            }
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn move_down(&mut self, n: usize) {
        if self.len() == 0 {
            return;
        }
        let i = (self.state.selected().unwrap_or(0) + n).min(self.len() - 1);
        self.state.select(Some(i));
    }

    pub fn move_up(&mut self, n: usize) {
        if self.len() == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        self.state.select(Some(cur.saturating_sub(n)));
    }

    pub fn home(&mut self) {
        if self.len() > 0 {
            self.state.select(Some(0));
        }
    }

    pub fn end(&mut self) {
        if self.len() > 0 {
            self.state.select(Some(self.len() - 1));
        }
    }

    /// Jump to the first entry whose name contains `query` (case-insensitive).
    pub fn search(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        let q = query.to_lowercase();
        if let Some(i) = self.entries.iter().position(|e| e.name.to_lowercase().contains(&q)) {
            self.state.select(Some(i));
        }
    }

    /// Index of the next/previous playable (non-dir) file relative to `path`.
    pub fn neighbour_file(&self, path: &Path, forward: bool) -> Option<PathBuf> {
        let files: Vec<&Entry> = self.entries.iter().filter(|e| !e.is_dir).collect();
        let cur = files.iter().position(|e| e.path == path)?;
        let next = if forward {
            cur + 1
        } else {
            cur.checked_sub(1)?
        };
        files.get(next).map(|e| e.path.clone())
    }
}
