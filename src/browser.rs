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
            if let Some(idx) = self
                .entries
                .iter()
                .position(|e| !e.is_parent && e.path == from)
            {
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
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.name.to_lowercase().contains(&q))
        {
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

    /// The first playable (non-directory) file in this directory, if any.
    pub fn first_file(&self) -> Option<PathBuf> {
        self.entries
            .iter()
            .find(|e| !e.is_dir)
            .map(|e| e.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::Browser;
    use std::fs;
    use tempfile::tempdir;

    fn names(b: &Browser) -> Vec<String> {
        b.entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.name.clone())
            .collect()
    }

    /// A minimal valid SMF: PPQ 480, 120 BPM, one quarter long (0.5 s).
    fn minimal_midi() -> Vec<u8> {
        let mut v = b"MThd".to_vec();
        v.extend_from_slice(&6u32.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes()); // format
        v.extend_from_slice(&1u16.to_be_bytes()); // ntracks
        v.extend_from_slice(&480u16.to_be_bytes()); // division
        let events: &[u8] = &[
            0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo 120
            0x83, 0x60, 0xFF, 0x2F, 0x00, // delta 480, end of track
        ];
        v.extend_from_slice(b"MTrk");
        v.extend_from_slice(&(events.len() as u32).to_be_bytes());
        v.extend_from_slice(events);
        v
    }

    #[test]
    fn filters_sorts_and_marks_parent() {
        let d = tempdir().unwrap();
        let p = d.path();
        fs::create_dir(p.join("zsub")).unwrap();
        fs::create_dir(p.join("asub")).unwrap();
        fs::write(p.join("b.mid"), b"x").unwrap();
        fs::write(p.join("a.midi"), b"xx").unwrap();
        fs::write(p.join("c.mid"), b"xxx").unwrap();
        fs::write(p.join("note.txt"), b"nope").unwrap();
        fs::write(p.join(".hidden.mid"), b"hidden").unwrap();

        let b = Browser::new(p.to_path_buf(), &["mid", "midi"], false);
        // Dirs first (alphabetical), then files (alphabetical); .txt and the
        // dotfile are excluded.
        assert_eq!(names(&b), ["asub", "zsub", "a.midi", "b.mid", "c.mid"]);
        assert!(b.entries[0].is_parent, "first entry should be '..'");
    }

    #[test]
    fn hidden_toggle_reveals_dotfiles() {
        let d = tempdir().unwrap();
        fs::write(d.path().join(".secret.mid"), b"x").unwrap();
        fs::write(d.path().join("plain.mid"), b"x").unwrap();
        let mut b = Browser::new(d.path().to_path_buf(), &["mid"], false);
        assert_eq!(names(&b), ["plain.mid"]);
        b.show_hidden = true;
        b.refresh();
        assert_eq!(names(&b), [".secret.mid", "plain.mid"]);
    }

    #[test]
    fn populates_size_and_duration() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("song.mid"), minimal_midi()).unwrap();
        let b = Browser::new(d.path().to_path_buf(), &["mid"], true);
        let e = b.entries.iter().find(|e| e.name == "song.mid").unwrap();
        assert_eq!(e.size, minimal_midi().len() as u64);
        let dur = e.duration.expect("duration should be computed");
        assert!((dur - 0.5).abs() < 1e-6, "{dur}");
    }

    #[test]
    fn duration_skipped_when_disabled() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("song.mid"), minimal_midi()).unwrap();
        let b = Browser::new(d.path().to_path_buf(), &["mid"], false);
        let e = b.entries.iter().find(|e| e.name == "song.mid").unwrap();
        assert!(e.duration.is_none());
    }

    #[test]
    fn neighbour_file_walks_files_only() {
        let d = tempdir().unwrap();
        for n in ["a.mid", "b.mid", "c.mid"] {
            fs::write(d.path().join(n), b"x").unwrap();
        }
        let b = Browser::new(d.path().to_path_buf(), &["mid"], false);
        let (a, bb, c) = (
            d.path().join("a.mid"),
            d.path().join("b.mid"),
            d.path().join("c.mid"),
        );
        assert_eq!(b.neighbour_file(&bb, true), Some(c.clone()));
        assert_eq!(b.neighbour_file(&bb, false), Some(a.clone()));
        assert_eq!(b.neighbour_file(&a, false), None);
        assert_eq!(b.neighbour_file(&c, true), None);
    }

    #[test]
    fn search_jumps_to_match() {
        let d = tempdir().unwrap();
        for n in ["alpha.mid", "bravo.mid", "charlie.mid"] {
            fs::write(d.path().join(n), b"x").unwrap();
        }
        let mut b = Browser::new(d.path().to_path_buf(), &["mid"], false);
        b.search("CHAR"); // case-insensitive
        assert_eq!(b.selected().unwrap().name, "charlie.mid");
    }

    #[test]
    fn enter_and_go_up_navigate() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        let mut b = Browser::new(d.path().to_path_buf(), &["mid"], false);
        let idx = b.entries.iter().position(|e| e.name == "sub").unwrap();
        b.state.select(Some(idx));
        b.enter_dir();
        assert_eq!(b.dir, d.path().join("sub"));
        b.go_up();
        assert_eq!(b.dir, d.path().to_path_buf());
    }
}
