//! Playing history: which MIDI file was heard through which SoundFont, when,
//! and how often.
//!
//! One chronological log is kept, most recent first. Each record is a
//! *combination* — the same track played through two SoundFonts is two records,
//! which is exactly the comparison voxfont exists to make. The per-track and
//! per-SoundFont views the UI offers are grouped projections of this one log,
//! so they can never drift apart.
//!
//! Entries are [`Location`]s, so a file inside a zip archive is remembered as
//! such (never as the temporary file it was extracted to) and stays playable
//! from the history on a later run.
//!
//! Stored next to the session file, as
//! `$XDG_CONFIG_HOME/voxfont/history.conf` (default `~/.config/...`), in the
//! same `key = value` style. Records are stanzas rather than one delimited line
//! each because [`Location::encode`] already uses tabs internally; giving every
//! value its own line means nothing has to be escaped, and unknown keys are
//! ignored so the format can gain fields without breaking older builds.

use crate::vfs::Location;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One remembered (track, SoundFont) combination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The MIDI file. `None` for a SoundFont loaded while nothing was playing.
    pub midi: Option<Location>,
    /// The SoundFont it was heard through. Always set in practice, since
    /// playback requires one.
    pub soundfont: Option<Location>,
    /// Unix seconds of the most recent play of this combination.
    pub when: u64,
    /// How many times this combination has been played.
    pub plays: u32,
}

impl Entry {
    fn key(&self) -> (Option<&Location>, Option<&Location>) {
        (self.midi.as_ref(), self.soundfont.as_ref())
    }
}

/// Which projection of the log the history overlay shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// Every remembered combination, newest first.
    Pairs,
    /// One row per MIDI file, newest first.
    Tracks,
    /// One row per SoundFont, newest first.
    Fonts,
}

impl View {
    pub fn next(self) -> View {
        match self {
            View::Pairs => View::Tracks,
            View::Tracks => View::Fonts,
            View::Fonts => View::Pairs,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            View::Pairs => "recent",
            View::Tracks => "tracks",
            View::Fonts => "soundfonts",
        }
    }
}

/// One rendered row of a view. `idxs` are the log entries it stands for: a
/// single entry in [`View::Pairs`], the whole group in the grouped views, so
/// "forget this row" removes exactly what the row displays.
#[derive(Clone, Debug)]
pub struct Row {
    pub midi: Option<Location>,
    pub soundfont: Option<Location>,
    pub when: u64,
    pub plays: u32,
    pub idxs: Vec<usize>,
    /// True when a side of the row can no longer be played, so the UI can flag
    /// it. Checked while the rows are built — on opening the overlay, changing
    /// its view or typing in its filter — never per rendered frame.
    pub gone: bool,
}

/// The log plus where it is persisted. A history with no `path` (the default,
/// and what `--no-history` leaves in place) keeps entries for the session only
/// and never writes to disk.
#[derive(Default)]
pub struct History {
    entries: Vec<Entry>,
    path: Option<PathBuf>,
    dirty: bool,
}

impl History {
    /// Read the history from the user's config directory. Only `main` calls
    /// this, so tests and `--no-history` never touch the real file.
    pub fn load() -> History {
        match history_path() {
            Some(p) => History::load_at(p),
            None => History::default(),
        }
    }

    /// Read (or start) a history stored at `path`.
    pub fn load_at(path: PathBuf) -> History {
        let mut entries = std::fs::read_to_string(&path)
            .map(|t| parse(&t))
            .unwrap_or_default();
        // Newest first, whatever order the file happened to be in.
        entries.sort_by_key(|e| std::cmp::Reverse(e.when));
        History {
            entries,
            path: Some(path),
            dirty: false,
        }
    }

    /// Write the log out if it changed and a path is set. Failures are silent:
    /// a history that cannot be saved must not break playback.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let path = match &self.path {
            Some(p) => p.clone(),
            None => return,
        };
        if write_atomically(&path, &serialize(&self.entries)).is_ok() {
            self.dirty = false;
        }
    }

    /// Remember that `midi` was played through `soundfont` at `now`.
    ///
    /// An existing record for the same combination moves back to the top and
    /// takes the new timestamp. `count` adds to its play tally; it is false for
    /// an automatic repeat of the track already at the top, so leaving a track
    /// looping does not inflate the count of a single listening session.
    pub fn record(
        &mut self,
        midi: Option<Location>,
        soundfont: Option<Location>,
        now: u64,
        count: bool,
    ) {
        if midi.is_none() && soundfont.is_none() {
            return;
        }
        let key = (midi.as_ref(), soundfont.as_ref());
        match self.entries.iter().position(|e| e.key() == key) {
            Some(i) => {
                let mut e = self.entries.remove(i);
                e.when = now;
                if count {
                    e.plays = e.plays.saturating_add(1);
                }
                self.entries.insert(0, e);
            }
            None => self.entries.insert(
                0,
                Entry {
                    midi,
                    soundfont,
                    when: now,
                    plays: 1,
                },
            ),
        }
        // Plays arrive in order, so the new entry is normally already on top;
        // sorting keeps the newest-first invariant true anyway — after a clock
        // change, or when an entry is re-recorded with an older timestamp. The
        // sort is stable, so equal timestamps keep the newest at the front.
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.when));
        self.dirty = true;
    }

    /// Drop the given log entries (the indices behind one displayed row).
    pub fn forget(&mut self, idxs: &[usize]) {
        let mut idxs: Vec<usize> = idxs.to_vec();
        idxs.sort_unstable();
        for i in idxs.into_iter().rev() {
            if i < self.entries.len() {
                self.entries.remove(i);
                self.dirty = true;
            }
        }
    }

    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.dirty = true;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build the rows of `view`, keeping only those matching the
    /// case-insensitive `filter` (matched against what the row displays).
    pub fn rows(&self, view: View, filter: &str) -> Vec<Row> {
        let rows = match view {
            View::Pairs => self
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| Row {
                    midi: e.midi.clone(),
                    soundfont: e.soundfont.clone(),
                    when: e.when,
                    plays: e.plays,
                    idxs: vec![i],
                    gone: is_gone(&e.midi, &e.soundfont),
                })
                .collect(),
            View::Tracks => self.grouped(|e| e.midi.clone()),
            View::Fonts => self.grouped(|e| e.soundfont.clone()),
        };
        let f = filter.trim().to_lowercase();
        if f.is_empty() {
            return rows;
        }
        rows.into_iter()
            .filter(|r| row_haystack(r).contains(&f))
            .collect()
    }

    /// Group the log by `key` (entries without one are skipped), preserving the
    /// newest-first order. A group shows the newest entry's counterpart and the
    /// summed play count.
    fn grouped<F: Fn(&Entry) -> Option<Location>>(&self, key: F) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        let mut seen: HashMap<Location, usize> = HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            let k = match key(e) {
                Some(k) => k,
                None => continue,
            };
            match seen.get(&k) {
                Some(&r) => {
                    let row: &mut Row = &mut rows[r];
                    row.plays = row.plays.saturating_add(e.plays);
                    row.idxs.push(i);
                }
                None => {
                    seen.insert(k, rows.len());
                    rows.push(Row {
                        midi: e.midi.clone(),
                        soundfont: e.soundfont.clone(),
                        when: e.when,
                        plays: e.plays,
                        idxs: vec![i],
                        gone: is_gone(&e.midi, &e.soundfont),
                    });
                }
            }
        }
        rows
    }
}

/// True when either side of a row has disappeared from disk, and playing it
/// again would fail.
fn is_gone(midi: &Option<Location>, soundfont: &Option<Location>) -> bool {
    let missing = |l: &Option<Location>| l.as_ref().map(|l| !l.exists()).unwrap_or(false);
    missing(midi) || missing(soundfont)
}

/// The text a filter is matched against: the two names the row displays.
fn row_haystack(r: &Row) -> String {
    let name = |l: &Option<Location>| l.as_ref().map(|l| l.file_name()).unwrap_or_default();
    format!("{} {}", name(&r.midi), name(&r.soundfont)).to_lowercase()
}

fn config_dir() -> Option<PathBuf> {
    crate::state::config_dir()
}

fn history_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("history.conf"))
}

/// Write via a temporary file in the same directory plus a rename, so an
/// interrupted write cannot truncate an accumulated history.
fn write_atomically(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("conf.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Parse the stanza file body. Unknown keys and malformed records are skipped
/// rather than rejected, so a file written by a newer (or interrupted) version
/// still loads.
fn parse(text: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut cur: Option<Entry> = None;
    let finish = |cur: &mut Option<Entry>, out: &mut Vec<Entry>| {
        if let Some(e) = cur.take() {
            if e.when > 0 && (e.midi.is_some() || e.soundfont.is_some()) {
                out.push(e);
            }
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[play]" {
            finish(&mut cur, &mut out);
            cur = Some(Entry {
                midi: None,
                soundfont: None,
                when: 0,
                plays: 1,
            });
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
            "when" => entry.when = val.parse().unwrap_or(0),
            "plays" => entry.plays = val.parse().unwrap_or(1).max(1),
            "midi" => entry.midi = Some(Location::decode(val)),
            "sf" => entry.soundfont = Some(Location::decode(val)),
            _ => {}
        }
    }
    finish(&mut cur, &mut out);
    out
}

/// Render the log, newest first, with each timestamp also spelled out as a
/// comment so the file is readable as it stands.
fn serialize(entries: &[Entry]) -> String {
    let mut out =
        String::from("# voxfont playing history v1\n# Newest first. Times below are local.\n");
    for e in entries {
        out.push_str(&format!("\n# {}\n[play]\n", fmt_stamp(e.when, true)));
        out.push_str(&format!("when = {}\n", e.when));
        out.push_str(&format!("plays = {}\n", e.plays));
        if let Some(m) = &e.midi {
            out.push_str(&format!("midi = {}\n", m.encode()));
        }
        if let Some(s) = &e.soundfont {
            out.push_str(&format!("sf = {}\n", s.encode()));
        }
    }
    out
}

/// Seconds since the Unix epoch, now.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A timestamp in the user's local time: "2026-09-17 14:03" (plus seconds when
/// `with_secs`). Falls back to UTC if the C library cannot give an offset.
pub fn fmt_stamp(unix: u64, with_secs: bool) -> String {
    fmt_stamp_at(unix, local_offset_secs(unix), with_secs)
}

/// Pure formatter: `unix` shifted by `offset` seconds, rendered as civil time.
fn fmt_stamp_at(unix: u64, offset: i64, with_secs: bool) -> String {
    let t = unix as i64 + offset;
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if with_secs {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
    }
}

/// How long ago `when` was, for the detail line: "just now", "7 min ago", ...
pub fn fmt_age(now: u64, when: u64) -> String {
    let secs = now.saturating_sub(when);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// Days since 1970-01-01 to (year, month, day), by Howard Hinnant's
/// `civil_from_days`. Proleptic Gregorian, valid far beyond any plausible
/// timestamp, and pure integer arithmetic — no date dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds east of UTC at `unix`, DST included, from the C library's
/// `localtime_r`. There is no local-time support in `std`, and the project
/// keeps its dependency list bare, so this reads the one field it needs from
/// `struct tm` (whose layout is stable on the glibc/musl Linux targets voxfont
/// builds for). Returns 0 — i.e. UTC — if the call fails.
// `c_long` is the same as `i64` on the 64-bit targets voxfont ships for, where
// clippy rightly calls the conversion redundant — but it is `i32` on a 32-bit
// one, so the cast has to stay.
#[allow(clippy::unnecessary_cast)]
fn local_offset_secs(unix: u64) -> i64 {
    use std::os::raw::{c_char, c_int, c_long};

    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
    }

    extern "C" {
        fn localtime_r(time: *const c_long, result: *mut Tm) -> *mut Tm;
    }

    let t: c_long = unix.min(c_long::MAX as u64) as c_long;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    let ok = unsafe { localtime_r(&t, &mut tm) };
    if ok.is_null() {
        0
    } else {
        tm.gmtoff as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(p: &str) -> Option<Location> {
        Some(Location::Fs(PathBuf::from(p)))
    }

    fn zip(archive: &str, inner: &str) -> Option<Location> {
        Some(Location::Zip {
            archive: PathBuf::from(archive),
            inner: inner.to_string(),
        })
    }

    #[test]
    fn newest_first_and_one_record_per_combination() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 100, true);
        h.record(fs("/m/b.mid"), fs("/s/one.sf2"), 200, true);
        // Same track, different font: a separate record.
        h.record(fs("/m/a.mid"), fs("/s/two.sf2"), 300, true);
        assert_eq!(h.len(), 3);

        let names: Vec<String> = h
            .rows(View::Pairs, "")
            .iter()
            .map(|r| r.midi.as_ref().unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.mid", "b.mid", "a.mid"]);

        // Replaying an old combination moves it to the top and counts the play.
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 400, true);
        assert_eq!(h.len(), 3);
        let top = &h.rows(View::Pairs, "")[0];
        assert_eq!(top.when, 400);
        assert_eq!(top.plays, 2);
        assert_eq!(top.soundfont.as_ref().unwrap().file_name(), "one.sf2");
    }

    #[test]
    fn newest_stays_on_top_even_out_of_order() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 300, true);
        // An older play recorded afterwards (a clock that went backwards)
        // must not end up above it.
        h.record(fs("/m/b.mid"), fs("/s/one.sf2"), 100, true);
        let whens: Vec<u64> = h.rows(View::Pairs, "").iter().map(|r| r.when).collect();
        assert_eq!(whens, [300, 100]);
    }

    #[test]
    fn uncounted_replay_refreshes_time_only() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 100, true);
        // A repeat-mode loop of the same track: newer, but not a new play.
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 160, false);
        let top = &h.rows(View::Pairs, "")[0];
        assert_eq!((top.when, top.plays), (160, 1));
    }

    #[test]
    fn nothing_to_remember_is_not_recorded() {
        let mut h = History::default();
        h.record(None, None, 100, true);
        assert!(h.is_empty());
    }

    #[test]
    fn grouped_views_summarise_the_log() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 100, true);
        h.record(fs("/m/b.mid"), fs("/s/two.sf2"), 200, true);
        h.record(fs("/m/a.mid"), fs("/s/two.sf2"), 300, true);
        // A SoundFont loaded with nothing playing: no track.
        h.record(None, fs("/s/three.sf2"), 400, true);

        // Tracks: newest first, one row per file, plays summed across fonts,
        // showing the font it was last heard through.
        let tracks = h.rows(View::Tracks, "");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].midi.as_ref().unwrap().file_name(), "a.mid");
        assert_eq!(tracks[0].soundfont.as_ref().unwrap().file_name(), "two.sf2");
        assert_eq!(tracks[0].plays, 2);
        assert_eq!(tracks[0].idxs.len(), 2, "row stands for both its entries");
        assert_eq!(tracks[1].midi.as_ref().unwrap().file_name(), "b.mid");

        // Fonts: the font-only record appears here but not in Tracks.
        let fonts = h.rows(View::Fonts, "");
        let names: Vec<String> = fonts
            .iter()
            .map(|r| r.soundfont.as_ref().unwrap().file_name())
            .collect();
        assert_eq!(names, ["three.sf2", "two.sf2", "one.sf2"]);
    }

    #[test]
    fn filter_matches_either_name_case_insensitively() {
        let mut h = History::default();
        h.record(fs("/m/CANYON.MID"), fs("/s/one.sf2"), 100, true);
        h.record(fs("/m/popcorn.mid"), fs("/s/Roland.sf2"), 200, true);

        assert_eq!(h.rows(View::Pairs, "canyon").len(), 1);
        assert_eq!(h.rows(View::Pairs, "roland").len(), 1);
        assert_eq!(h.rows(View::Pairs, "sf2").len(), 2);
        assert!(h.rows(View::Pairs, "nothing").is_empty());
    }

    #[test]
    fn forget_removes_every_entry_behind_a_row() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 100, true);
        h.record(fs("/m/b.mid"), fs("/s/one.sf2"), 200, true);
        h.record(fs("/m/a.mid"), fs("/s/two.sf2"), 300, true);

        let row = h.rows(View::Tracks, "")[0].clone(); // a.mid, both fonts
        h.forget(&row.idxs);
        assert_eq!(h.len(), 1);
        assert_eq!(
            h.rows(View::Pairs, "")[0]
                .midi
                .as_ref()
                .unwrap()
                .file_name(),
            "b.mid"
        );

        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn round_trips_through_serialize_and_parse() {
        let entries = vec![
            Entry {
                midi: zip("/m/songs.zip", "classics/canyon.mid"),
                soundfont: fs("/s/one.sf2"),
                when: 1_758_067_400,
                plays: 3,
            },
            // A SoundFont loaded with nothing playing.
            Entry {
                midi: None,
                soundfont: zip("/s/banks.zip", "gm/font.sf2"),
                when: 1_758_060_000,
                plays: 1,
            },
        ];
        assert_eq!(parse(&serialize(&entries)), entries);
    }

    #[test]
    fn parse_skips_junk_unknown_keys_and_incomplete_records() {
        let text = "\
# a comment

[play]
when = 100
plays = 2
midi = /m/a.mid
sf = /s/one.sf2
future_field = whatever

[play]
sf = /s/two.sf2

[play]
when = 300
sf = /s/three.sf2
";
        let got = parse(text);
        // The middle record has no timestamp, so it is dropped.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].midi, fs("/m/a.mid"));
        assert_eq!(got[0].plays, 2);
        assert_eq!(got[1].soundfont, fs("/s/three.sf2"));
        assert_eq!(got[1].plays, 1, "missing plays defaults to one");
        assert!(parse("").is_empty());
    }

    #[test]
    fn saves_and_reloads_from_disk() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("history.conf");

        let mut h = History::load_at(path.clone());
        assert!(h.is_empty(), "a missing file starts an empty history");
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 1_758_067_400, true);
        h.save();

        let again = History::load_at(path.clone());
        assert_eq!(again.len(), 1);
        assert_eq!(again.rows(View::Pairs, "")[0].when, 1_758_067_400);
        // No leftovers from the atomic write.
        assert!(!path.with_extension("conf.tmp").exists());
    }

    #[test]
    fn in_memory_history_never_writes() {
        let mut h = History::default();
        h.record(fs("/m/a.mid"), fs("/s/one.sf2"), 100, true);
        h.save(); // no path: must be a no-op, not a panic
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn stamps_are_absolute_and_offset_aware() {
        assert_eq!(fmt_stamp_at(0, 0, true), "1970-01-01 00:00:00");
        assert_eq!(fmt_stamp_at(1_758_067_400, 0, false), "2025-09-17 00:03");
        // +2h offset shifts the clock, and a negative offset can cross midnight.
        assert_eq!(fmt_stamp_at(1_758_067_400, 7200, false), "2025-09-17 02:03");
        assert_eq!(
            fmt_stamp_at(1_758_067_400, -3600, false),
            "2025-09-16 23:03"
        );
        // Leap day, to exercise the civil-date arithmetic.
        assert_eq!(fmt_stamp_at(1_709_164_800, 0, false), "2024-02-29 00:00");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(fmt_age(1000, 1000), "just now");
        assert_eq!(fmt_age(1000, 950), "just now");
        assert_eq!(fmt_age(1000, 400), "10 min ago");
        assert_eq!(fmt_age(100_000, 20_000), "22 h ago");
        // Just past a day, the unit changes.
        assert_eq!(fmt_age(100_000, 10_000), "1 d ago");
        assert_eq!(fmt_age(1_000_000, 100_000), "10 d ago");
        // A clock that jumped backwards must not panic.
        assert_eq!(fmt_age(100, 500), "just now");
    }
}
