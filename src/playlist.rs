//! Playlists: user-authored lists of MIDI tracks, each optionally pinned to a
//! SoundFont, stored as extended M3U files.
//!
//! Unlike the [favourites](crate::favourites), which are always (track, font)
//! pairs and live in voxfont's own config directory, a playlist is a document
//! the user owns: it can sit anywhere, be written by hand in a text editor, and
//! mix items that bring their own SoundFont with items that play through the
//! user's font (see `App::user_font`). The format is documented in full in the
//! README ("Playlist file format"); in short:
//!
//! ```text
//! #EXTM3U
//! #PLAYLIST:Evening A/B
//! #VOXFONT:sf=/srv/sf2/CT8MGM.SF2
//! ~/midi/CANYON.MID
//! classics/bwv1041.mid
//! ```
//!
//! Every non-comment line is one item. `#VOXFONT:sf=<path>` pins a SoundFont to
//! the *next* item only; an item without one has no font of its own. Relative
//! paths resolve against the playlist's directory, and a path running through a
//! `.zip` file addresses a member of that archive.
//!
//! Lines voxfont does not act on — comments, `#EXTINF` and other players' tags,
//! tracks that are not MIDI files — are kept with the item that follows them and
//! written back on save, so saving a hand-edited file loses nothing.

use crate::app::{MIDI_EXTS, SF2_EXTS};
use crate::history::{self, History, Row};
use crate::vfs::Location;
use std::path::{Component, Path, PathBuf};

/// File extensions recognised as playlists.
pub const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8"];

/// The per-item SoundFont directive.
const SF_DIRECTIVE: &str = "#VOXFONT:sf=";
/// Standard extended-M3U title directive.
const TITLE_DIRECTIVE: &str = "#PLAYLIST:";

/// One entry: a track, and the SoundFont it must be heard through if it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub midi: Location,
    /// `None` plays the item through the user's font.
    pub soundfont: Option<Location>,
    /// Lines that preceded the item in the file without being acted on, written
    /// back in front of it so notes and foreign tags move with their item.
    pub extra: Vec<String>,
}

/// A parsed playlist file, before items are given their session identities.
#[derive(Default, Debug, PartialEq, Eq)]
struct Doc {
    title: Option<String>,
    /// Comment lines at the top of the file, separated from the first item by a
    /// blank line. They stay at the top however the items are reordered.
    header: Vec<String>,
    items: Vec<Item>,
    /// Lines after the last item.
    trailer: Vec<String>,
    /// How many lines were not acted on (kept, but not played).
    ignored: usize,
}

/// The open playlist. The default is an empty, unsaved list with no path.
#[derive(Default)]
pub struct Playlist {
    /// Each item with a session-only identity, so the queue can follow the
    /// playing item through reorders and removals even when the same track and
    /// font appear more than once.
    entries: Vec<(u64, Item)>,
    next_id: u64,
    path: Option<PathBuf>,
    title: Option<String>,
    header: Vec<String>,
    trailer: Vec<String>,
    /// True when the list differs from the file. Saving is explicit, so this
    /// is what stands between a quit and lost edits.
    dirty: bool,
}

impl Playlist {
    /// Read the playlist at `path`. Returns it with the number of lines that
    /// were kept but not acted on, for the status line.
    pub fn load(path: &Path) -> Result<(Playlist, usize), String> {
        let path = absolute(path);
        let bytes =
            std::fs::read(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let doc = parse(&text, &base_dir(&path));
        let ignored = doc.ignored;
        let mut p = Playlist {
            path: Some(path),
            title: doc.title,
            header: doc.header,
            trailer: doc.trailer,
            ..Playlist::default()
        };
        for item in doc.items {
            p.insert(item);
        }
        Ok((p, ignored))
    }

    /// Write the list back to its own file.
    pub fn save(&mut self) -> Result<(), String> {
        match self.path.clone() {
            Some(p) => self.save_as(p),
            None => Err("The playlist has no file yet".into()),
        }
    }

    /// Write the list to `path`, which becomes its file from now on. Relative
    /// paths inside are recomputed against the new location.
    pub fn save_as(&mut self, path: PathBuf) -> Result<(), String> {
        let path = absolute(&path);
        let body = self.serialize(&base_dir(&path));
        crate::state::write_atomically(&path, &body)
            .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
        self.path = Some(path);
        self.dirty = false;
        Ok(())
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The name to show: the file's `#PLAYLIST:` title, else its file name.
    pub fn name(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            None => "untitled".into(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&Item> {
        self.entries.get(idx).map(|(_, item)| item)
    }

    /// The session identity of the item at `idx`.
    pub fn id_at(&self, idx: usize) -> Option<u64> {
        self.entries.get(idx).map(|(id, _)| *id)
    }

    /// Where the item with identity `id` sits now, if it is still in the list.
    pub fn position_of(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|(i, _)| *i == id)
    }

    /// Append a track, with a SoundFont of its own or without one. Locations
    /// are made absolute, so a panel opened on a relative directory still
    /// writes paths that resolve from wherever the playlist file is.
    pub fn push(&mut self, midi: Location, soundfont: Option<Location>) {
        self.insert(Item {
            midi: absolute_loc(&midi),
            soundfont: soundfont.map(|s| absolute_loc(&s)),
            extra: Vec::new(),
        });
        self.dirty = true;
    }

    /// Drop the item at `idx`.
    pub fn remove(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries.remove(idx);
            self.dirty = true;
        }
    }

    /// Exchange two items, which is how the overlay reorders the list.
    pub fn swap(&mut self, a: usize, b: usize) {
        let n = self.entries.len();
        if a < n && b < n && a != b {
            self.entries.swap(a, b);
            self.dirty = true;
        }
    }

    /// Pin a SoundFont to the item at `idx`, or (with `None`) unpin it so it
    /// plays through the user's font.
    pub fn set_font(&mut self, idx: usize, soundfont: Option<Location>) {
        if let Some((_, item)) = self.entries.get_mut(idx) {
            let soundfont = soundfont.map(|s| absolute_loc(&s));
            if item.soundfont != soundfont {
                item.soundfont = soundfont;
                self.dirty = true;
            }
        }
    }

    /// Build the displayed rows, keeping only those matching the
    /// case-insensitive `filter`. `user_font` is what an item without a font
    /// plays through, used to look up its play count; the row itself keeps
    /// `soundfont: None` so the overlay can say so.
    pub fn rows(&self, filter: &str, history: &History, user_font: Option<&Location>) -> Vec<Row> {
        let f = filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .map(|(i, (_, item))| {
                let midi = Some(item.midi.clone());
                let soundfont = item.soundfont.clone();
                let heard_through = soundfont.as_ref().or(user_font);
                Row {
                    when: 0,
                    plays: heard_through
                        .map(|sf| history.plays_of(&item.midi, sf))
                        .unwrap_or(0),
                    idxs: vec![i],
                    gone: history::is_gone(&midi, &soundfont),
                    midi,
                    soundfont,
                }
            })
            .filter(|r| f.is_empty() || history::row_haystack(r).contains(&f))
            .collect()
    }

    fn insert(&mut self, item: Item) {
        self.entries.push((self.next_id, item));
        self.next_id += 1;
    }

    /// Render the file, with paths relative to `base` where they are below it.
    fn serialize(&self, base: &Path) -> String {
        let mut out = String::from("#EXTM3U\n");
        if let Some(t) = &self.title {
            out.push_str(&format!("{TITLE_DIRECTIVE}{t}\n"));
        }
        for line in &self.header {
            out.push_str(line);
            out.push('\n');
        }
        // The blank line is what keeps the header at the top on the next read.
        if !self.header.is_empty() {
            out.push('\n');
        }
        for (_, item) in &self.entries {
            for line in &item.extra {
                out.push_str(line);
                out.push('\n');
            }
            if let Some(sf) = &item.soundfont {
                out.push_str(&format!("{SF_DIRECTIVE}{}\n", path_text(sf, base)));
            }
            out.push_str(&path_text(&item.midi, base));
            out.push('\n');
        }
        for line in &self.trailer {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// True when `name` has a playlist extension.
pub fn is_playlist_name(name: &str) -> bool {
    has_ext(name, PLAYLIST_EXTS)
}

fn has_ext(name: &str, exts: &[&str]) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Parse a playlist body. `base` is the directory relative paths resolve from.
fn parse(text: &str, base: &Path) -> Doc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut doc = Doc::default();
    // Lines waiting for the item they belong to.
    let mut pending: Vec<String> = Vec::new();
    // A SoundFont directive waiting for its track, with its line as written.
    let mut font: Option<(Location, String)> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            // A blank line before the first item closes off the file header.
            if doc.items.is_empty() {
                doc.header.append(&mut pending);
            }
            continue;
        }
        if line == "#EXTM3U" {
            continue;
        }
        if let Some(t) = line.strip_prefix(TITLE_DIRECTIVE) {
            let t = t.trim();
            doc.title = (!t.is_empty()).then(|| t.to_string());
            continue;
        }
        if let Some(value) = line.strip_prefix(SF_DIRECTIVE) {
            // A second directive before any track leaves the first with nothing
            // to apply to: it is kept as a line, but not acted on.
            if let Some((_, prev)) = font.take() {
                pending.push(prev);
                doc.ignored += 1;
            }
            let value = value.trim();
            if has_ext(value, SF2_EXTS) {
                font = Some((resolve(value, base), line.to_string()));
            } else {
                pending.push(line.to_string());
                doc.ignored += 1;
            }
            continue;
        }
        if line.starts_with('#') {
            pending.push(line.to_string());
            continue;
        }
        if !has_ext(line, MIDI_EXTS) {
            // Not something voxfont can play (an audio file from another
            // player's list, say). Kept for that player; skipped here.
            pending.push(line.to_string());
            doc.ignored += 1;
            continue;
        }
        doc.items.push(Item {
            midi: resolve(line, base),
            soundfont: font.take().map(|(loc, _)| loc),
            extra: std::mem::take(&mut pending),
        });
    }
    if let Some((_, prev)) = font.take() {
        pending.push(prev);
        doc.ignored += 1;
    }
    doc.trailer = pending;
    doc
}

/// Resolve a path as written in a playlist: `~` is the home directory, a
/// relative path is relative to `base`, and a path through an existing `.zip`
/// file addresses a member of that archive.
fn resolve(text: &str, base: &Path) -> Location {
    let p = PathBuf::from(crate::app::expand_tilde(text));
    let p = if p.is_absolute() { p } else { base.join(p) };
    split_archive(&normalize(&p))
}

/// Split `p` at the first component that is an existing `.zip` *file*, making
/// the rest the member path. A real directory named `x.zip` is not an archive.
fn split_archive(p: &Path) -> Location {
    let comps: Vec<Component> = p.components().collect();
    let mut prefix = PathBuf::new();
    for (i, c) in comps.iter().enumerate() {
        prefix.push(c);
        let is_zip = c
            .as_os_str()
            .to_str()
            .map(|s| s.to_ascii_lowercase().ends_with(".zip"))
            .unwrap_or(false);
        if is_zip && i + 1 < comps.len() && prefix.is_file() {
            let inner = comps[i + 1..]
                .iter()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            return Location::Zip {
                archive: prefix,
                inner,
            };
        }
    }
    Location::Fs(p.to_path_buf())
}

/// Resolve `.` and `..` lexically, so a path written as `../x.mid` names the
/// same location the panels list, rather than a different spelling of it.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` made absolute against the working directory, lexically normalised.
pub fn absolute(path: &Path) -> PathBuf {
    normalize(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn absolute_loc(loc: &Location) -> Location {
    match loc {
        Location::Fs(p) => Location::Fs(absolute(p)),
        Location::Zip { archive, inner } => Location::Zip {
            archive: absolute(archive),
            inner: inner.clone(),
        },
    }
}

/// The directory a playlist's relative paths resolve from.
fn base_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// A location as written in a playlist at `base`: relative when it is below
/// `base`, absolute otherwise, with an archive member spelled as a path running
/// through the archive.
fn path_text(loc: &Location, base: &Path) -> String {
    let (path, inner) = match loc {
        Location::Fs(p) => (p.as_path(), ""),
        Location::Zip { archive, inner } => (archive.as_path(), inner.as_str()),
    };
    let mut s = match path.strip_prefix(base) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    };
    if !inner.is_empty() {
        s.push('/');
        s.push_str(inner);
    }
    // A relative name that would read back as a comment or a home path.
    if s.starts_with('#') || s == "~" || s.starts_with("~/") {
        s.insert_str(0, "./");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn fs(p: &str) -> Location {
        Location::Fs(PathBuf::from(p))
    }

    fn item(midi: &str, sf: Option<&str>) -> Item {
        Item {
            midi: fs(midi),
            soundfont: sf.map(fs),
            extra: Vec::new(),
        }
    }

    #[test]
    fn a_font_directive_applies_to_the_next_item_only() {
        let doc = parse(
            "#EXTM3U\n\
             #VOXFONT:sf=/s/one.sf2\n\
             /m/a.mid\n\
             /m/b.mid\n\
             #VOXFONT:sf=/s/two.sf2\n\
             /m/a.mid\n",
            Path::new("/lists"),
        );
        assert_eq!(
            doc.items,
            vec![
                item("/m/a.mid", Some("/s/one.sf2")),
                item("/m/b.mid", None),
                // The same track again is a separate item.
                item("/m/a.mid", Some("/s/two.sf2")),
            ]
        );
        assert_eq!(doc.ignored, 0);
    }

    #[test]
    fn a_plain_m3u_is_a_list_of_items_without_fonts() {
        let doc = parse("/m/a.mid\r\n/m/b.MID\r\n", Path::new("/lists"));
        assert_eq!(
            doc.items,
            vec![item("/m/a.mid", None), item("/m/b.MID", None)]
        );
        assert_eq!(doc.title, None);
    }

    #[test]
    fn relative_and_home_paths_resolve() {
        let doc = parse(
            "\u{feff}#VOXFONT:sf=fonts/one.sf2\nsub/a.mid\n../up.mid\n./c.mid\n",
            Path::new("/lists/evening"),
        );
        assert_eq!(doc.items[0].midi, fs("/lists/evening/sub/a.mid"));
        assert_eq!(
            doc.items[0].soundfont,
            Some(fs("/lists/evening/fonts/one.sf2"))
        );
        assert_eq!(doc.items[1].midi, fs("/lists/up.mid"));
        assert_eq!(doc.items[2].midi, fs("/lists/evening/c.mid"));

        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let doc = parse("~/midi/x.mid\n", Path::new("/lists"));
            assert_eq!(
                doc.items[0].midi,
                Location::Fs(PathBuf::from(&home).join("midi/x.mid"))
            );
        }
    }

    #[test]
    fn a_path_through_a_zip_file_is_an_archive_member() {
        let d = tempfile::tempdir().unwrap();
        let zip = d.path().join("songs.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
            w.start_file("pop/popcorn.mid", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"x").unwrap();
            w.finish().unwrap();
        }
        // A directory that merely ends in .zip is still a directory.
        std::fs::create_dir(d.path().join("dir.zip")).unwrap();

        let doc = parse("songs.zip/pop/popcorn.mid\ndir.zip/a.mid\n", d.path());
        assert_eq!(
            doc.items[0].midi,
            Location::Zip {
                archive: zip,
                inner: "pop/popcorn.mid".into()
            }
        );
        assert_eq!(
            doc.items[1].midi,
            Location::Fs(d.path().join("dir.zip/a.mid"))
        );
    }

    #[test]
    fn title_and_header_are_read() {
        let doc = parse(
            "#EXTM3U\n#PLAYLIST: Evening A/B \n# my notes\n\n# about a\n/m/a.mid\n",
            Path::new("/"),
        );
        assert_eq!(doc.title.as_deref(), Some("Evening A/B"));
        // The comment before the blank line is the file's; the one after is
        // the first item's.
        assert_eq!(doc.header, vec!["# my notes"]);
        assert_eq!(doc.items[0].extra, vec!["# about a"]);
    }

    #[test]
    fn lines_not_acted_on_are_counted_and_kept() {
        let doc = parse(
            "#VOXFONT:sf=/s/lost.sf2\n\
             #VOXFONT:sf=/s/one.sf2\n\
             #EXTINF:123,Canyon\n\
             /m/a.mid\n\
             /music/song.mp3\n\
             #VOXFONT:sf=/s/readme.txt\n\
             /m/b.mid\n\
             #VOXFONT:sf=/s/dangling.sf2\n",
            Path::new("/"),
        );
        assert_eq!(doc.items.len(), 2);
        // The first directive was superseded before it reached a track.
        assert_eq!(doc.items[0].soundfont, Some(fs("/s/one.sf2")));
        assert_eq!(
            doc.items[0].extra,
            vec!["#VOXFONT:sf=/s/lost.sf2", "#EXTINF:123,Canyon"]
        );
        // Not a MIDI file, and not a SoundFont: both kept, neither acted on.
        assert_eq!(doc.items[1].soundfont, None);
        assert_eq!(
            doc.items[1].extra,
            vec!["/music/song.mp3", "#VOXFONT:sf=/s/readme.txt"]
        );
        // A directive with no track after it.
        assert_eq!(doc.trailer, vec!["#VOXFONT:sf=/s/dangling.sf2"]);
        assert_eq!(doc.ignored, 4);
    }

    #[test]
    fn a_font_directive_may_be_separated_from_its_track() {
        let doc = parse(
            "#VOXFONT:sf=/s/one.sf2\n\n# the good one\n/m/a.mid\n",
            Path::new("/"),
        );
        assert_eq!(doc.items[0].soundfont, Some(fs("/s/one.sf2")));
        assert_eq!(doc.ignored, 0);
    }

    #[test]
    fn serialize_writes_relative_paths_below_the_file_and_absolute_elsewhere() {
        let mut p = Playlist::default();
        p.push(fs("/lists/sub/a.mid"), Some(fs("/srv/one.sf2")));
        p.push(fs("/elsewhere/b.mid"), None);
        p.push(fs("/lists/#odd.mid"), None);
        p.push(
            Location::Zip {
                archive: PathBuf::from("/lists/songs.zip"),
                inner: "pop/c.mid".into(),
            },
            None,
        );
        let text = p.serialize(Path::new("/lists"));
        assert_eq!(
            text,
            "#EXTM3U\n\
             #VOXFONT:sf=/srv/one.sf2\n\
             sub/a.mid\n\
             /elsewhere/b.mid\n\
             ./#odd.mid\n\
             songs.zip/pop/c.mid\n"
        );
    }

    #[test]
    fn round_trips_through_serialize_and_parse() {
        let text = "#EXTM3U\n\
                    #PLAYLIST:Evening\n\
                    # top\n\
                    \n\
                    #EXTINF:1,x\n\
                    #VOXFONT:sf=/s/one.sf2\n\
                    /m/a.mid\n\
                    rel/b.mid\n\
                    # end\n";
        let doc = parse(text, Path::new("/lists"));
        let mut p = Playlist {
            title: doc.title.clone(),
            header: doc.header.clone(),
            trailer: doc.trailer.clone(),
            ..Playlist::default()
        };
        for item in doc.items.clone() {
            p.insert(item);
        }
        let again = p.serialize(Path::new("/lists"));
        assert_eq!(again, text, "a canonical file is written back unchanged");
        assert_eq!(parse(&again, Path::new("/lists")), doc);
    }

    #[test]
    fn edits_mark_the_list_dirty_and_ids_follow_items() {
        let mut p = Playlist::default();
        assert!(!p.is_dirty());
        p.push(fs("/m/a.mid"), None);
        p.push(fs("/m/a.mid"), None);
        p.push(fs("/m/b.mid"), Some(fs("/s/one.sf2")));
        assert!(p.is_dirty());

        // Duplicates are distinct items with distinct identities.
        let (first, second) = (p.id_at(0).unwrap(), p.id_at(1).unwrap());
        assert_ne!(first, second);
        p.swap(0, 2);
        assert_eq!(p.position_of(first), Some(2));
        p.remove(2);
        assert_eq!(p.position_of(first), None);
        assert_eq!(p.position_of(second), Some(1));

        p.set_font(0, None);
        assert_eq!(p.get(0).unwrap().soundfont, None);
        // Out of range is ignored rather than panicking.
        p.swap(0, 9);
        p.remove(9);
        p.set_font(9, None);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn saves_and_reloads_from_disk() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("evening.m3u");
        let mut p = Playlist::default();
        assert!(p.save().is_err(), "a new list has no file yet");
        p.push(Location::Fs(d.path().join("a.mid")), None);
        p.push(
            Location::Fs(d.path().join("b.mid")),
            Some(Location::Fs(d.path().join("one.sf2"))),
        );
        p.save_as(path.clone()).unwrap();
        assert!(!p.is_dirty());
        assert_eq!(p.name(), "evening.m3u");

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\n#VOXFONT:sf=one.sf2\nb.mid\n"), "{body}");

        let (again, ignored) = Playlist::load(&path).unwrap();
        assert_eq!(ignored, 0);
        assert_eq!(again.len(), 2);
        assert_eq!(
            again.get(0).unwrap().midi,
            Location::Fs(d.path().join("a.mid"))
        );
        assert_eq!(
            again.get(1).unwrap().soundfont,
            Some(Location::Fs(d.path().join("one.sf2")))
        );
        assert!(!again.is_dirty());
        assert!(Playlist::load(&d.path().join("missing.m3u")).is_err());
    }

    #[test]
    fn rows_count_plays_through_the_font_the_item_uses() {
        let mut p = Playlist::default();
        p.push(fs("/m/a.mid"), Some(fs("/s/one.sf2")));
        p.push(fs("/m/b.mid"), None);
        let mut h = History::default();
        h.record(Some(fs("/m/a.mid")), Some(fs("/s/one.sf2")), 10, true);
        h.record(Some(fs("/m/b.mid")), Some(fs("/s/two.sf2")), 20, true);

        let rows = p.rows("", &h, Some(&fs("/s/two.sf2")));
        assert_eq!(rows[0].plays, 1);
        assert_eq!(rows[1].plays, 1, "counted through the user's font");
        assert_eq!(rows[1].soundfont, None, "the row still says it has none");
        assert_eq!(p.rows("", &h, None)[1].plays, 0);

        assert_eq!(p.rows("B.MID", &h, None).len(), 1);
        assert_eq!(p.rows("b.mid", &h, None)[0].idxs, vec![1]);
    }

    #[test]
    fn playlist_names_are_recognised() {
        assert!(is_playlist_name("a.m3u"));
        assert!(is_playlist_name("A.M3U8"));
        assert!(!is_playlist_name("a.mid"));
        assert!(!is_playlist_name("m3u"));
    }
}
