//! A single directory-browser panel: lists subdirectories and files matching a
//! set of extensions, with a movable selection. Directories and archives are
//! addressed uniformly through [`Location`], so a zip is browsed exactly like a
//! directory.

use crate::midi;
use crate::vfs::{self, Location, ZipCache};
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Entry {
    pub name: String,
    pub loc: Location,
    pub is_dir: bool,
    /// True for the synthetic ".." parent entry.
    pub is_parent: bool,
    /// File size in bytes (0 for directories and for archive members).
    pub size: u64,
    /// Duration in seconds for MIDI files, when computed.
    pub duration: Option<f64>,
}

pub struct Browser {
    dir: Location,
    pub entries: Vec<Entry>,
    pub state: ListState,
    /// Lower-case extensions (without dot) that should be shown as files.
    exts: Vec<String>,
    pub show_hidden: bool,
    /// When true, parse MIDI files to report their duration.
    compute_duration: bool,
    /// When true, `.zip` files are shown as enterable directories. Only the
    /// MIDI panel browses archives; the SoundFont panel leaves them out.
    allow_archives: bool,
    /// Cache of parsed MIDI durations, keyed by path (filesystem files only).
    duration_cache: HashMap<PathBuf, Option<f64>>,
    /// Per-archive directory indexes, so navigating within a zip is cheap.
    zip_cache: ZipCache,
}

impl Browser {
    /// Create a browser starting at `dir`, which may be a filesystem directory
    /// or a location inside a saved archive.
    pub fn new_at(
        dir: Location,
        exts: &[&str],
        compute_duration: bool,
        allow_archives: bool,
    ) -> Browser {
        let mut b = Browser {
            dir,
            entries: Vec::new(),
            state: ListState::default(),
            exts: exts.iter().map(|s| s.to_lowercase()).collect(),
            show_hidden: false,
            compute_duration,
            allow_archives,
            duration_cache: HashMap::new(),
            zip_cache: ZipCache::new(),
        };
        b.refresh();
        b
    }

    pub fn refresh(&mut self) {
        let mut dirs: Vec<Entry> = Vec::new();
        let mut files: Vec<Entry> = Vec::new();

        for c in vfs::read_dir(&self.dir, &mut self.zip_cache, self.allow_archives) {
            if !self.show_hidden && c.name.starts_with('.') {
                continue;
            }
            if c.is_dir {
                dirs.push(Entry {
                    name: c.name,
                    loc: c.loc,
                    is_dir: true,
                    is_parent: false,
                    size: 0,
                    duration: None,
                });
            } else if self.matches_ext(&c.name) {
                // Duration is only computed for filesystem files. Inside an
                // archive it would mean decompressing every member just to draw
                // the list, so the duration column is left blank there.
                let duration = match (self.compute_duration, c.loc.as_fs()) {
                    (true, Some(p)) => {
                        let p = p.to_path_buf();
                        *self
                            .duration_cache
                            .entry(p.clone())
                            .or_insert_with(|| midi::parse(&p).map(|i| i.duration_secs))
                    }
                    _ => None,
                };
                files.push(Entry {
                    name: c.name,
                    loc: c.loc,
                    is_dir: false,
                    is_parent: false,
                    size: c.size,
                    duration,
                });
            }
        }

        let key = |e: &Entry| e.name.to_lowercase();
        dirs.sort_by_key(key);
        files.sort_by_key(key);

        let mut entries = Vec::with_capacity(dirs.len() + files.len() + 1);
        if let Some(parent) = self.dir.parent() {
            entries.push(Entry {
                name: "..".to_string(),
                loc: parent,
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

    fn matches_ext(&self, name: &str) -> bool {
        match Path::new(name).extension().and_then(|e| e.to_str()) {
            Some(e) => self.exts.iter().any(|x| x == &e.to_lowercase()),
            None => false,
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.entries.get(i))
    }

    /// Display path of the current directory (for panel titles).
    pub fn dir_display(&self) -> String {
        self.dir.display()
    }

    /// The current directory location (for session persistence).
    pub fn location(&self) -> Location {
        self.dir.clone()
    }

    /// Move the cursor onto the entry at `loc`, if it is in the current listing.
    pub fn select_loc(&mut self, loc: &Location) {
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| !e.is_parent && &e.loc == loc)
        {
            self.state.select(Some(i));
        }
    }

    /// The nearest real filesystem directory, for session persistence and the
    /// "go to directory" prompt (which only operate on the filesystem).
    pub fn fs_dir(&self) -> PathBuf {
        self.dir.fs_dir()
    }

    fn set_loc(&mut self, loc: Location) {
        self.dir = loc;
        self.state.select(Some(0));
        self.refresh();
    }

    pub fn set_dir(&mut self, dir: PathBuf) {
        self.set_loc(Location::Fs(dir));
    }

    /// Enter the directory (or archive) at the cursor.
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.selected() {
            if e.is_dir {
                let target = e.loc.clone();
                self.set_loc(target);
            }
        }
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.dir.parent() {
            let from = self.dir.clone();
            self.set_loc(parent);
            // Land the cursor on the directory we just came from.
            if let Some(idx) = self
                .entries
                .iter()
                .position(|e| !e.is_parent && e.loc == from)
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

    /// The next/previous playable (non-dir) file relative to `loc`.
    pub fn neighbour_file(&self, loc: &Location, forward: bool) -> Option<Location> {
        let files: Vec<&Entry> = self.entries.iter().filter(|e| !e.is_dir).collect();
        let cur = files.iter().position(|e| &e.loc == loc)?;
        let next = if forward {
            cur + 1
        } else {
            cur.checked_sub(1)?
        };
        files.get(next).map(|e| e.loc.clone())
    }

    /// The first playable (non-directory) file in this directory, if any.
    pub fn first_file(&self) -> Option<Location> {
        self.entries.iter().find(|e| !e.is_dir).map(|e| e.loc.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::Browser;
    use crate::vfs::Location;
    use std::fs;
    use std::io::Write as _;
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

        let b = Browser::new_at(Location::Fs(p.to_path_buf()), &["mid", "midi"], false, false);
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
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        assert_eq!(names(&b), ["plain.mid"]);
        b.show_hidden = true;
        b.refresh();
        assert_eq!(names(&b), [".secret.mid", "plain.mid"]);
    }

    #[test]
    fn populates_size_and_duration() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("song.mid"), minimal_midi()).unwrap();
        let b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], true, false);
        let e = b.entries.iter().find(|e| e.name == "song.mid").unwrap();
        assert_eq!(e.size, minimal_midi().len() as u64);
        let dur = e.duration.expect("duration should be computed");
        assert!((dur - 0.5).abs() < 1e-6, "{dur}");
    }

    #[test]
    fn duration_skipped_when_disabled() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("song.mid"), minimal_midi()).unwrap();
        let b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let e = b.entries.iter().find(|e| e.name == "song.mid").unwrap();
        assert!(e.duration.is_none());
    }

    #[test]
    fn neighbour_file_walks_files_only() {
        let d = tempdir().unwrap();
        for n in ["a.mid", "b.mid", "c.mid"] {
            fs::write(d.path().join(n), b"x").unwrap();
        }
        let b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let loc = |n: &str| Location::Fs(d.path().join(n));
        let (a, bb, c) = (loc("a.mid"), loc("b.mid"), loc("c.mid"));
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
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        b.search("CHAR"); // case-insensitive
        assert_eq!(b.selected().unwrap().name, "charlie.mid");
    }

    #[test]
    fn enter_and_go_up_navigate() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let idx = b.entries.iter().position(|e| e.name == "sub").unwrap();
        b.state.select(Some(idx));
        b.enter_dir();
        assert_eq!(b.dir, Location::Fs(d.path().join("sub")));
        b.go_up();
        assert_eq!(b.dir, Location::Fs(d.path().to_path_buf()));
    }

    /// Build a zip with the given members (dirs end in '/') at `path`.
    fn make_zip(path: &std::path::Path, names: &[&str]) {
        use zip::write::SimpleFileOptions;
        let f = fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        for n in names {
            if n.ends_with('/') {
                w.add_directory(n.trim_end_matches('/'), opts).unwrap();
            } else {
                w.start_file(*n, opts).unwrap();
                w.write_all(b"x").unwrap();
            }
        }
        w.finish().unwrap();
    }

    #[test]
    fn select_loc_restores_cursor_including_zip_members() {
        let d = tempdir().unwrap();
        for n in ["a.mid", "b.mid", "c.mid"] {
            fs::write(d.path().join(n), b"x").unwrap();
        }
        make_zip(&d.path().join("z.zip"), &["inner/song.mid"]);

        // Filesystem file: cursor lands on the saved location.
        let mut b = Browser::new_at(
            Location::Fs(d.path().to_path_buf()),
            &["mid"],
            false,
            true,
        );
        b.select_loc(&Location::Fs(d.path().join("b.mid")));
        assert_eq!(b.selected().unwrap().name, "b.mid");

        // A location not in the current listing leaves the cursor put.
        b.select_loc(&Location::Fs(d.path().join("missing.mid")));
        assert_eq!(b.selected().unwrap().name, "b.mid");

        // The same works for a member inside a zip: open the subdir, then place
        // the cursor on the saved member (this is the launch-time restore path).
        let mut z = Browser::new_at(
            Location::Zip {
                archive: d.path().join("z.zip"),
                inner: "inner".to_string(),
            },
            &["mid"],
            false,
            true,
        );
        z.select_loc(&Location::Zip {
            archive: d.path().join("z.zip"),
            inner: "inner/song.mid".to_string(),
        });
        assert_eq!(z.selected().unwrap().name, "song.mid");
    }

    #[test]
    fn browses_into_zip_and_back_out() {
        let d = tempdir().unwrap();
        make_zip(&d.path().join("songs.zip"), &["a/x.mid", "b.mid", "n.txt"]);
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, true);

        // The archive appears in the filesystem listing as a directory.
        let zi = b.entries.iter().position(|e| e.name == "songs.zip").unwrap();
        assert!(b.entries[zi].is_dir);
        b.state.select(Some(zi));
        b.enter_dir();

        // Inside the zip root: the .txt is filtered out, "a/" shows as a dir.
        assert_eq!(names(&b), ["a", "b.mid"]);
        assert!(matches!(b.dir, Location::Zip { ref inner, .. } if inner.is_empty()));

        // Stepping back out of the archive lands on the archive entry again.
        b.go_up();
        assert_eq!(b.dir, Location::Fs(d.path().to_path_buf()));
        assert_eq!(b.selected().unwrap().name, "songs.zip");
    }
}
