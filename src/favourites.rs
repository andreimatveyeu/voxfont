//! Favourites: the (track, SoundFont) pairs the user starred, in the order they
//! play as a playlist.
//!
//! A favourite is always a *pair*. Hearing a tune through a particular font is
//! the judgement voxfont exists to support, so a starred track without its font
//! — or a font without its track — would record half an opinion. The same tune
//! starred with three fonts is three favourites, and the playlist then plays it
//! three times over, once per font.
//!
//! This is a separate store from the playing [history](crate::history), not a
//! flag on it: history rows are pruned with `d`/`D`, are skipped entirely under
//! `--no-history`, and only appear after a few seconds of playback, none of
//! which should be able to lose a deliberate star. Play counts are the one thing
//! not duplicated here — the overlay reads them back out of the history through
//! [`History::plays_of`](crate::history::History::plays_of).
//!
//! Stored beside the session file as
//! `$XDG_CONFIG_HOME/voxfont/favourites.conf` (default `~/.config/...`), in the
//! same stanza style as the history, and for the same reason: nothing has to be
//! escaped, and unknown keys are ignored so the format can gain fields (a note,
//! a rating) without breaking older builds.
//!
//! Unlike the history, the file's order is meaningful — it is the playlist
//! order — so it is never re-sorted on load.

use crate::history::{self, History, Row};
use crate::vfs::Location;
use std::collections::HashSet;
use std::path::PathBuf;

/// One starred (track, SoundFont) pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fav {
    pub midi: Location,
    pub soundfont: Location,
    /// Unix seconds when it was starred.
    pub added: u64,
}

impl Fav {
    fn key(&self) -> (Location, Location) {
        (self.midi.clone(), self.soundfont.clone())
    }
}

/// The starred pairs plus where they are persisted. A store with no `path` (the
/// default) keeps them for the session only and never writes to disk, which is
/// what every test uses.
#[derive(Default)]
pub struct Favourites {
    entries: Vec<Fav>,
    path: Option<PathBuf>,
    dirty: bool,
    /// Membership indexes, rebuilt on every change. The panels ask about every
    /// visible row on every frame, so these lookups have to be O(1) rather than
    /// a scan of the list.
    pairs: HashSet<(Location, Location)>,
    tracks: HashSet<Location>,
    fonts: HashSet<Location>,
}

impl Favourites {
    /// Read the favourites from the user's config directory. Only `main` calls
    /// this, so tests never touch the real file.
    pub fn load() -> Favourites {
        match favourites_path() {
            Some(p) => Favourites::load_at(p),
            None => Favourites::default(),
        }
    }

    /// Read (or start) a store held at `path`.
    pub fn load_at(path: PathBuf) -> Favourites {
        let entries = std::fs::read_to_string(&path)
            .map(|t| parse(&t))
            .unwrap_or_default();
        let mut f = Favourites {
            entries,
            path: Some(path),
            dirty: false,
            ..Favourites::default()
        };
        f.reindex();
        f
    }

    /// Write the list out if it changed and a path is set. Failures are silent:
    /// a star that cannot be saved must not break playback.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let path = match &self.path {
            Some(p) => p.clone(),
            None => return,
        };
        if crate::state::write_atomically(&path, &serialize(&self.entries)).is_ok() {
            self.dirty = false;
        }
    }

    /// Star the pair, or un-star it if it is already starred. Returns true when
    /// it was added. New favourites go to the end, since the order is the
    /// playlist order and a star should not reshuffle what is queued.
    pub fn toggle(&mut self, midi: Location, soundfont: Location, now: u64) -> bool {
        let key = (midi.clone(), soundfont.clone());
        match self.entries.iter().position(|f| f.key() == key) {
            Some(i) => {
                self.entries.remove(i);
                self.reindex();
                self.dirty = true;
                false
            }
            None => {
                self.entries.push(Fav {
                    midi,
                    soundfont,
                    added: now,
                });
                self.reindex();
                self.dirty = true;
                true
            }
        }
    }

    /// Drop the favourite at `idx` (the entry behind one displayed row).
    pub fn remove(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.entries.remove(idx);
            self.reindex();
            self.dirty = true;
        }
    }

    /// Exchange two entries, which is how the overlay reorders the playlist.
    pub fn swap(&mut self, a: usize, b: usize) {
        let n = self.entries.len();
        if a < n && b < n && a != b {
            self.entries.swap(a, b);
            self.dirty = true;
        }
    }

    /// True when this exact pair is starred.
    pub fn is_pair(&self, midi: &Location, soundfont: &Location) -> bool {
        self.pairs.contains(&(midi.clone(), soundfont.clone()))
    }

    /// True when the track is starred with *some* font — the hollow star in the
    /// MIDI panel, which says "you have starred this tune, but not through the
    /// font loaded right now".
    pub fn has_track(&self, midi: &Location) -> bool {
        self.tracks.contains(midi)
    }

    /// True when the font is starred with *some* track.
    pub fn has_font(&self, soundfont: &Location) -> bool {
        self.fonts.contains(soundfont)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&Fav> {
        self.entries.get(idx)
    }

    /// Where this pair sits in the playlist, if it is starred.
    pub fn position_of(&self, midi: &Location, soundfont: &Location) -> Option<usize> {
        self.entries
            .iter()
            .position(|f| &f.midi == midi && &f.soundfont == soundfont)
    }

    /// Build the displayed rows, keeping only those matching the
    /// case-insensitive `filter`. There is one row per favourite in list order:
    /// no grouped views, because collapsing pairs would hide exactly what the
    /// list is for and would leave reordering with no well-defined meaning.
    ///
    /// `history` supplies the play counts; a pair never played shows none.
    pub fn rows(&self, filter: &str, history: &History) -> Vec<Row> {
        let rows: Vec<Row> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let midi = Some(f.midi.clone());
                let soundfont = Some(f.soundfont.clone());
                Row {
                    when: f.added,
                    plays: history.plays_of(&f.midi, &f.soundfont),
                    idxs: vec![i],
                    gone: history::is_gone(&midi, &soundfont),
                    midi,
                    soundfont,
                }
            })
            .collect();
        let f = filter.trim().to_lowercase();
        if f.is_empty() {
            return rows;
        }
        rows.into_iter()
            .filter(|r| history::row_haystack(r).contains(&f))
            .collect()
    }

    fn reindex(&mut self) {
        self.pairs = self.entries.iter().map(|f| f.key()).collect();
        self.tracks = self.entries.iter().map(|f| f.midi.clone()).collect();
        self.fonts = self.entries.iter().map(|f| f.soundfont.clone()).collect();
    }
}

fn favourites_path() -> Option<PathBuf> {
    crate::state::config_dir().map(|d| d.join("favourites.conf"))
}

/// Parse the stanza file body. A record missing either side is not a pair, so
/// it is skipped rather than half-restored; unknown keys are ignored.
fn parse(text: &str) -> Vec<Fav> {
    let mut out: Vec<Fav> = Vec::new();
    let mut cur: Option<(Option<Location>, Option<Location>, u64)> = None;
    let finish = |cur: &mut Option<(Option<Location>, Option<Location>, u64)>,
                  out: &mut Vec<Fav>| {
        if let Some((midi, soundfont, added)) = cur.take() {
            if let (Some(midi), Some(soundfont)) = (midi, soundfont) {
                out.push(Fav {
                    midi,
                    soundfont,
                    added,
                });
            }
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[fav]" {
            finish(&mut cur, &mut out);
            cur = Some((None, None, 0));
            continue;
        }
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let entry = match cur.as_mut() {
            Some(e) => e,
            None => continue,
        };
        let val = val.trim();
        match key.trim() {
            "added" => entry.2 = val.parse().unwrap_or(0),
            "midi" => entry.0 = Some(Location::decode(val)),
            "sf" => entry.1 = Some(Location::decode(val)),
            _ => {}
        }
    }
    finish(&mut cur, &mut out);
    out
}

/// Render the list in playlist order, each timestamp also spelled out as a
/// comment so the file is readable as it stands.
fn serialize(entries: &[Fav]) -> String {
    let mut out = String::from(
        "# voxfont favourites v1\n\
         # Track + SoundFont pairs, in the order they play. Times below are local.\n",
    );
    for f in entries {
        out.push_str(&format!(
            "\n# {}\n[fav]\n",
            history::fmt_stamp(f.added, true)
        ));
        out.push_str(&format!("added = {}\n", f.added));
        out.push_str(&format!("midi = {}\n", f.midi.encode()));
        out.push_str(&format!("sf = {}\n", f.soundfont.encode()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(p: &str) -> Location {
        Location::Fs(PathBuf::from(p))
    }

    fn zip(archive: &str, inner: &str) -> Location {
        Location::Zip {
            archive: PathBuf::from(archive),
            inner: inner.to_string(),
        }
    }

    #[test]
    fn toggle_stars_then_unstars() {
        let mut f = Favourites::default();
        assert!(f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100));
        assert_eq!(f.len(), 1);
        assert!(f.is_pair(&fs("/m/a.mid"), &fs("/s/one.sf2")));

        // The same pair again takes the star back off.
        assert!(!f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 200));
        assert!(f.is_empty());
        assert!(!f.is_pair(&fs("/m/a.mid"), &fs("/s/one.sf2")));
        assert!(!f.has_track(&fs("/m/a.mid")));
    }

    #[test]
    fn same_track_through_two_fonts_is_two_favourites() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/a.mid"), fs("/s/two.sf2"), 200);
        assert_eq!(f.len(), 2);
        // Both pairs are starred, and the track is starred either way.
        assert!(f.is_pair(&fs("/m/a.mid"), &fs("/s/one.sf2")));
        assert!(f.is_pair(&fs("/m/a.mid"), &fs("/s/two.sf2")));
        assert!(f.has_track(&fs("/m/a.mid")));
        // A third font is not starred, though the track and font each are.
        assert!(!f.is_pair(&fs("/m/a.mid"), &fs("/s/three.sf2")));
        assert!(f.has_font(&fs("/s/two.sf2")));
        assert!(!f.has_font(&fs("/s/three.sf2")));
    }

    #[test]
    fn new_stars_go_to_the_end_of_the_playlist() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/b.mid"), fs("/s/one.sf2"), 200);
        f.toggle(fs("/m/c.mid"), fs("/s/one.sf2"), 300);
        let names: Vec<String> = (0..f.len())
            .map(|i| f.get(i).unwrap().midi.file_name())
            .collect();
        assert_eq!(names, ["a.mid", "b.mid", "c.mid"]);
        assert_eq!(f.position_of(&fs("/m/b.mid"), &fs("/s/one.sf2")), Some(1));
        assert_eq!(f.position_of(&fs("/m/z.mid"), &fs("/s/one.sf2")), None);
    }

    #[test]
    fn swap_reorders_and_remove_drops_one() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/b.mid"), fs("/s/one.sf2"), 200);
        f.swap(0, 1);
        assert_eq!(f.get(0).unwrap().midi.file_name(), "b.mid");
        // Out-of-range and no-op swaps are ignored rather than panicking.
        f.swap(0, 9);
        f.swap(1, 1);
        assert_eq!(f.len(), 2);

        f.remove(0);
        assert_eq!(f.len(), 1);
        assert_eq!(f.get(0).unwrap().midi.file_name(), "a.mid");
        // The indexes follow the removal.
        assert!(!f.has_track(&fs("/m/b.mid")));
        f.remove(7);
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn rows_join_play_counts_from_the_history() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/b.mid"), fs("/s/one.sf2"), 200);

        let mut h = History::default();
        h.record(Some(fs("/m/a.mid")), Some(fs("/s/one.sf2")), 500, true);
        h.record(Some(fs("/m/a.mid")), Some(fs("/s/one.sf2")), 600, true);

        let rows = f.rows("", &h);
        assert_eq!(rows.len(), 2);
        // List order, not history order.
        assert_eq!(rows[0].midi, Some(fs("/m/a.mid")));
        assert_eq!(rows[0].plays, 2);
        assert_eq!(rows[0].when, 100, "the row's time is when it was starred");
        // Starred but never played: no count of its own to show.
        assert_eq!(rows[1].plays, 0);
        assert_eq!(rows[1].idxs, vec![1]);
    }

    #[test]
    fn rows_filter_matches_either_name_case_insensitively() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/CANYON.MID"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/popcorn.mid"), fs("/s/Roland.sf2"), 200);
        let h = History::default();

        assert_eq!(f.rows("canyon", &h).len(), 1);
        assert_eq!(f.rows("roland", &h).len(), 1);
        assert_eq!(f.rows("sf2", &h).len(), 2);
        assert!(f.rows("nothing", &h).is_empty());
        // A filtered row still knows its index in the full list.
        assert_eq!(f.rows("popcorn", &h)[0].idxs, vec![1]);
    }

    #[test]
    fn round_trips_through_serialize_and_parse() {
        let entries = vec![
            Fav {
                midi: zip("/m/songs.zip", "classics/canyon.mid"),
                soundfont: fs("/s/one.sf2"),
                added: 1_758_067_400,
            },
            Fav {
                midi: fs("/m/b.mid"),
                soundfont: zip("/s/banks.zip", "gm/font.sf2"),
                added: 1_758_060_000,
            },
        ];
        // Order is preserved exactly: it is the playlist.
        assert_eq!(parse(&serialize(&entries)), entries);
    }

    #[test]
    fn parse_skips_half_pairs_junk_and_unknown_keys() {
        let text = "\
# a comment

[fav]
added = 100
midi = /m/a.mid
sf = /s/one.sf2
rating = 5

[fav]
added = 200
midi = /m/no-font.mid

[fav]
added = 300
sf = /s/no-track.sf2

[fav]
midi = /m/c.mid
sf = /s/two.sf2
";
        let got = parse(text);
        // Only the complete pairs survive; the two half-records are dropped.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].midi, fs("/m/a.mid"));
        assert_eq!(got[0].added, 100);
        assert_eq!(got[1].midi, fs("/m/c.mid"));
        assert_eq!(got[1].added, 0, "a missing timestamp is not fatal");
        assert!(parse("").is_empty());
    }

    #[test]
    fn saves_and_reloads_from_disk_in_order() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("favourites.conf");

        let mut f = Favourites::load_at(path.clone());
        assert!(f.is_empty(), "a missing file starts an empty list");
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 1_758_067_400);
        f.toggle(fs("/m/b.mid"), fs("/s/two.sf2"), 1_758_067_500);
        f.save();

        let again = Favourites::load_at(path.clone());
        assert_eq!(again.len(), 2);
        assert_eq!(again.get(0).unwrap().midi.file_name(), "a.mid");
        // The membership indexes are rebuilt on load, not just on toggle.
        assert!(again.is_pair(&fs("/m/b.mid"), &fs("/s/two.sf2")));
        assert!(!path.with_extension("conf.tmp").exists());
    }

    #[test]
    fn a_reorder_survives_a_reload() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("favourites.conf");

        let mut f = Favourites::load_at(path.clone());
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.toggle(fs("/m/b.mid"), fs("/s/one.sf2"), 200);
        f.save();
        f.swap(0, 1);
        f.save();

        let again = Favourites::load_at(path);
        assert_eq!(again.get(0).unwrap().midi.file_name(), "b.mid");
        assert_eq!(again.get(1).unwrap().midi.file_name(), "a.mid");
    }

    #[test]
    fn in_memory_store_never_writes() {
        let mut f = Favourites::default();
        f.toggle(fs("/m/a.mid"), fs("/s/one.sf2"), 100);
        f.save(); // no path: must be a no-op, not a panic
        assert_eq!(f.len(), 1);
    }
}
