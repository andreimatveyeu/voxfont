//! A single directory-browser panel: lists subdirectories and files matching a
//! set of extensions, with a movable selection. Directories and archives are
//! addressed uniformly through [`Location`], so a zip is browsed exactly like a
//! directory.

use crate::midi;
use crate::vfs::{self, Location, ZipCache};
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
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
    /// The full directory/archive listing, before the search filter.
    all: Vec<Entry>,
    /// Visible listing: `all` narrowed to entries matching `filter`. The UI and
    /// all cursor operations work on this, so navigation stays within matches.
    pub entries: Vec<Entry>,
    /// Active case-insensitive name filter; empty means show everything.
    filter: String,
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
            all: Vec::new(),
            entries: Vec::new(),
            filter: String::new(),
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
                    // 0 for plain directories; an archive carries its file size.
                    size: c.size,
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

        self.all = entries;
        self.apply_filter();
    }

    /// Rebuild the visible `entries` from `all` by applying the current filter,
    /// keeping the cursor on the same item when it survives (otherwise the top).
    fn apply_filter(&mut self) {
        let prev = self.selected().map(|e| e.loc.clone());
        self.entries = if self.filter.is_empty() {
            self.all.clone()
        } else {
            self.all
                .iter()
                .filter(|e| e.name.to_lowercase().contains(&self.filter))
                .cloned()
                .collect()
        };
        if self.entries.is_empty() {
            self.state.select(None);
        } else {
            let idx = prev
                .and_then(|loc| self.entries.iter().position(|e| e.loc == loc))
                .unwrap_or(0);
            self.state.select(Some(idx));
        }
    }

    /// Set the case-insensitive name filter (empty clears it), narrowing the
    /// visible list live without re-reading the directory.
    pub fn set_filter(&mut self, query: &str) {
        self.filter = query.to_lowercase();
        self.apply_filter();
    }

    /// True while a search filter is narrowing the list.
    pub fn is_filtered(&self) -> bool {
        !self.filter.is_empty()
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

    /// Number of currently visible entries, excluding the synthetic ".." parent.
    /// With a filter active this is the match count.
    pub fn item_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_parent).count()
    }

    /// Total entries in the directory/archive (ignoring any filter), excluding
    /// the synthetic ".." parent.
    pub fn total_count(&self) -> usize {
        self.all.iter().filter(|e| !e.is_parent).count()
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

    /// Navigate to `loc`'s containing directory/archive (if not already there)
    /// and move the cursor onto it. Spans filesystem directories and zip
    /// archives, since the container is resolved through [`Location::parent`].
    pub fn reveal(&mut self, loc: &Location) {
        if let Some(parent) = loc.parent() {
            if self.dir != parent {
                self.set_loc(parent);
            }
            self.select_loc(loc);
        }
    }

    /// The nearest real filesystem directory, for session persistence and the
    /// "go to directory" prompt (which only operate on the filesystem).
    pub fn fs_dir(&self) -> PathBuf {
        self.dir.fs_dir()
    }

    fn set_loc(&mut self, loc: Location) {
        self.dir = loc;
        // A fresh directory always lists in full; drop any active filter.
        self.filter.clear();
        self.state.select(Some(0));
        self.refresh();
    }

    pub fn set_dir(&mut self, dir: PathBuf) {
        self.set_loc(Location::Fs(dir));
    }

    /// Enter the directory (or archive) at the cursor.
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.selected() {
            if e.is_parent {
                // Entering ".." is going up: land the cursor on the directory we
                // came from rather than leaving it parked on the new "..".
                self.go_up();
            } else if e.is_dir {
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

    /// The next/previous playable (non-dir) file relative to `loc`, resolved
    /// within `loc`'s own container directory/archive — *not* wherever the
    /// browser is currently pointed. This lets auto-advance keep playing
    /// through the directory of the playing file even after the user has
    /// browsed elsewhere. Returns `None` past the last/first file, or if the
    /// container is gone.
    pub fn neighbour_file(&mut self, loc: &Location, forward: bool) -> Option<Location> {
        let files = self.list_files_in(&loc.parent()?);
        let cur = files.iter().position(|l| l == loc)?;
        let next = if forward {
            cur + 1
        } else {
            cur.checked_sub(1)?
        };
        files.get(next).cloned()
    }

    /// The first playable (non-directory) file in `loc`'s container, if any.
    /// Used to loop back to the start of the playing file's directory.
    pub fn first_file_of(&mut self, loc: &Location) -> Option<Location> {
        self.list_files_in(&loc.parent()?).into_iter().next()
    }

    /// Sorted playable files in `dir`, honouring the hidden-file and extension
    /// rules but ignoring any active search filter and the browser's own
    /// cursor/current directory. Reads the container fresh, so it reflects the
    /// real listing of the playing file's directory or archive.
    fn list_files_in(&mut self, dir: &Location) -> Vec<Location> {
        let mut files: Vec<(String, Location)> = Vec::new();
        for c in vfs::read_dir(dir, &mut self.zip_cache, self.allow_archives) {
            if !self.show_hidden && c.name.starts_with('.') {
                continue;
            }
            if !c.is_dir && self.matches_ext(&c.name) {
                files.push((c.name.to_lowercase(), c.loc));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.into_iter().map(|(_, loc)| loc).collect()
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

        let b = Browser::new_at(
            Location::Fs(p.to_path_buf()),
            &["mid", "midi"],
            false,
            false,
        );
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
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let loc = |n: &str| Location::Fs(d.path().join(n));
        let (a, bb, c) = (loc("a.mid"), loc("b.mid"), loc("c.mid"));
        assert_eq!(b.neighbour_file(&bb, true), Some(c.clone()));
        assert_eq!(b.neighbour_file(&bb, false), Some(a.clone()));
        assert_eq!(b.neighbour_file(&a, false), None);
        assert_eq!(b.neighbour_file(&c, true), None);
    }

    #[test]
    fn neighbour_and_first_follow_the_playing_dir_not_the_browsed_one() {
        // Two directories, each with its own files. Start the browser in `play`
        // (where the "playing" file lives), then navigate it away into `other`.
        // Auto-advance must still resolve within `play`.
        let root = tempdir().unwrap();
        let play = root.path().join("play");
        let other = root.path().join("other");
        fs::create_dir(&play).unwrap();
        fs::create_dir(&other).unwrap();
        for n in ["1.mid", "2.mid", "3.mid"] {
            fs::write(play.join(n), b"x").unwrap();
        }
        fs::write(other.join("z.mid"), b"x").unwrap();

        let mut b = Browser::new_at(Location::Fs(play.clone()), &["mid"], false, false);
        // Browse away to the unrelated directory.
        b.set_dir(other.clone());

        let p = |n: &str| Location::Fs(play.join(n));
        // Next/previous still walk the playing file's own directory.
        assert_eq!(b.neighbour_file(&p("2.mid"), true), Some(p("3.mid")));
        assert_eq!(b.neighbour_file(&p("2.mid"), false), Some(p("1.mid")));
        // Past the end, repeat-mode wrap finds the first file of that dir.
        assert_eq!(b.neighbour_file(&p("3.mid"), true), None);
        assert_eq!(b.first_file_of(&p("3.mid")), Some(p("1.mid")));
    }

    #[test]
    fn filter_narrows_list_and_selects_first_match() {
        let d = tempdir().unwrap();
        for n in ["alpha.mid", "bravo.mid", "brazil.mid", "charlie.mid"] {
            fs::write(d.path().join(n), b"x").unwrap();
        }
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        assert_eq!(b.total_count(), 4);

        b.set_filter("BR"); // case-insensitive
        assert!(b.is_filtered());
        assert_eq!(names(&b), ["bravo.mid", "brazil.mid"]);
        assert_eq!(b.item_count(), 2);
        assert_eq!(b.total_count(), 4);
        // Cursor lands on the first match and can move within the filtered set.
        assert_eq!(b.selected().unwrap().name, "bravo.mid");
        b.move_down(1);
        assert_eq!(b.selected().unwrap().name, "brazil.mid");

        // Clearing restores the full list, keeping the cursor on the same item.
        b.set_filter("");
        assert!(!b.is_filtered());
        assert_eq!(b.item_count(), 4);
        assert_eq!(b.selected().unwrap().name, "brazil.mid");
    }

    #[test]
    fn filter_with_no_match_clears_selection() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("song.mid"), b"x").unwrap();
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        b.set_filter("zzz");
        assert!(b.selected().is_none());
        assert_eq!(b.item_count(), 0);
    }

    #[test]
    fn changing_directory_drops_the_filter() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub").join("inner.mid"), b"x").unwrap();
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        b.set_filter("zzz"); // hides everything
        let idx = b.all.iter().position(|e| e.name == "sub").unwrap();
        // Re-select "sub" in the unfiltered list to enter it.
        b.set_filter("");
        b.state.select(Some(idx));
        b.enter_dir();
        assert!(!b.is_filtered(), "entering a directory clears the filter");
        assert_eq!(names(&b), ["inner.mid"]);
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

    #[test]
    fn entering_parent_lands_on_previous_dir() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let idx = b.entries.iter().position(|e| e.name == "sub").unwrap();
        b.state.select(Some(idx));
        b.enter_dir();
        assert_eq!(b.dir, Location::Fs(d.path().join("sub")));

        // The cursor is on ".."; entering it should go up and highlight "sub",
        // not leave the cursor parked on the new "..".
        assert!(b.selected().unwrap().is_parent);
        b.enter_dir();
        assert_eq!(b.dir, Location::Fs(d.path().to_path_buf()));
        assert_eq!(b.selected().unwrap().name, "sub");
    }

    #[test]
    fn reveal_navigates_to_a_files_directory() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub").join("song.mid"), b"x").unwrap();
        fs::write(d.path().join("other.mid"), b"x").unwrap();

        // Start in the root, sitting on a different entry.
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, false);
        let target = Location::Fs(d.path().join("sub").join("song.mid"));
        b.reveal(&target);

        // It should have descended into "sub" and put the cursor on the file.
        assert_eq!(b.dir, Location::Fs(d.path().join("sub")));
        assert_eq!(b.selected().unwrap().loc, target);
    }

    #[test]
    fn reveal_descends_into_zip_member() {
        let d = tempdir().unwrap();
        let zip = d.path().join("songs.zip");
        make_zip(&zip, &["a/x.mid", "b.mid"]);

        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, true);
        let target = Location::Zip {
            archive: zip.clone(),
            inner: "a/x.mid".to_string(),
        };
        b.reveal(&target);

        assert_eq!(
            b.dir,
            Location::Zip {
                archive: zip,
                inner: "a".to_string()
            }
        );
        assert_eq!(b.selected().unwrap().loc, target);
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
        let mut b = Browser::new_at(Location::Fs(d.path().to_path_buf()), &["mid"], false, true);
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
        let zi = b
            .entries
            .iter()
            .position(|e| e.name == "songs.zip")
            .unwrap();
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
