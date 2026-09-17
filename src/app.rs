//! Application state and the controller logic that ties the two browser panels
//! to the fluidsynth player.

use crate::browser::Browser;
use crate::favourites::Favourites;
use crate::fluid::Synth;
use crate::history::{self, History, Row, View};
use crate::midi::{self, MidiInfo};
use crate::vfs::{self, Location};
use ratatui::widgets::ListState;
use rustyline::completion::{longest_common_prefix, Candidate, FilenameCompleter};
use std::path::PathBuf;
use std::time::Instant;
use tempfile::NamedTempFile;

pub const MIDI_EXTS: &[&str] = &["mid", "midi", "kar", "rmi"];
pub const SF2_EXTS: &[&str] = &["sf2", "sf3"];

/// How long a (track, SoundFont) combination must actually play before it is
/// written to the history. Paging through a directory with Enter fires a play
/// per keystroke; without this the history would fill with tracks nobody heard.
const MIN_LISTEN_SECS: f64 = 5.0;

#[derive(PartialEq, Clone, Copy)]
pub enum Panel {
    Midi,
    Sf2,
}

#[derive(PartialEq, Clone, Copy)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// The combination currently being listened to, waiting to clear
/// [`MIN_LISTEN_SECS`] before it is written to the history.
struct Pending {
    midi: Option<Location>,
    soundfont: Option<Location>,
    /// Value of the playback clock when this combination started, so the dwell
    /// is measured in playback time (pauses don't count) rather than wall time.
    at_secs: f64,
    /// False for a repeat-mode loop of the track already at the top of the
    /// history: it refreshes the timestamp without inflating the play count.
    count: bool,
}

/// Which store the overlay is showing. Both are lists of (track, SoundFont)
/// rows over the same layout and keys, so they share one overlay rather than
/// each growing their own copy of the navigation, filter and reveal logic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    History,
    Favourites,
}

/// Where the *next* track comes from when the current one finishes. Set by
/// whatever started playing: browsing plays on through the directory, a
/// favourite plays on through the favourites list. There is deliberately no
/// separate "playlist mode" to toggle — as with directory playback, where you
/// started decides what follows.
#[derive(Clone)]
enum PlaySource {
    Directory,
    /// Playing the favourites list. `key` identifies the entry that is playing
    /// so the position survives a reorder; `idx` is where it sat, used as a
    /// fallback if that entry is un-starred while it plays.
    Favourites {
        key: (Location, Location),
        idx: usize,
    },
}

/// The next thing to play, as resolved from the current [`PlaySource`].
struct Step {
    midi: Location,
    /// The SoundFont it must be heard through; `None` keeps the loaded one.
    soundfont: Option<Location>,
    /// Its index in the favourites list, when it came from there.
    fav_idx: Option<usize>,
}

/// State of the history / favourites overlay while it is open.
pub struct Overlay {
    pub source: Source,
    /// Which projection of the history is shown. Unused by the favourites,
    /// which are one flat list: grouping them would hide the individual pairs
    /// the list exists to hold, and leave reordering with no clear meaning.
    pub view: View,
    pub state: ListState,
    /// The rows matching `filter`, as currently displayed.
    pub rows: Vec<Row>,
    pub filter: String,
    /// True while `/` is capturing keystrokes into `filter`.
    pub filtering: bool,
    /// Set by the first `D`; the second one actually clears the history.
    confirm_clear: bool,
}

impl Overlay {
    pub fn selected(&self) -> Option<&Row> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    pub fn move_down(&mut self, n: usize) {
        if self.rows.is_empty() {
            return;
        }
        let i = (self.state.selected().unwrap_or(0) + n).min(self.rows.len() - 1);
        self.state.select(Some(i));
    }

    pub fn move_up(&mut self, n: usize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        self.state.select(Some(cur.saturating_sub(n)));
    }

    pub fn home(&mut self) {
        if !self.rows.is_empty() {
            self.state.select(Some(0));
        }
    }

    pub fn end(&mut self) {
        if !self.rows.is_empty() {
            self.state.select(Some(self.rows.len() - 1));
        }
    }
}

pub struct App {
    pub midi: Browser,
    pub sf2: Browser,
    pub active: Panel,
    pub synth: Synth,

    pub state: PlayState,
    pub now_playing: Option<Location>,
    pub soundfont: Option<Location>,
    /// The most recently played file, remembered across sessions so the cursor
    /// can return to it on launch. Unlike `now_playing` it is seeded from the
    /// saved session at startup (before anything is actually played).
    pub last_played: Option<Location>,
    /// Parsed metadata (division, time signature, duration) of the current track.
    pub cur_info: Option<MidiInfo>,

    /// What has been played, and through which SoundFont. Starts in-memory
    /// only; `main` swaps in the persisted one unless `--no-history` was given.
    pub history: History,
    /// The starred (track, SoundFont) pairs, which double as the playlist.
    /// Starts in-memory only; `main` swaps in the persisted one. `--no-history`
    /// does not affect it: a star is a deliberate act, not a recording.
    pub favs: Favourites,
    /// The combination being listened to right now, not yet committed.
    pending: Option<Pending>,
    /// Open history / favourites overlay, if any.
    pub overlay: Option<Overlay>,
    /// Where the next track comes from when this one ends.
    play_source: PlaySource,

    /// Temp files backing the current track / SoundFont when they come from an
    /// archive. Kept alive while in use; dropping them deletes the temp file.
    play_temp: Option<NamedTempFile>,
    sf_temp: Option<NamedTempFile>,

    pub volume: u8, // 0..=100
    /// Repeat mode: loop the current track (next off) or the directory (next on).
    pub repeat: bool,
    /// Next mode: advance to the next file in the directory when one finishes.
    pub next_mode: bool,

    pub message: Option<String>,
    /// Active incremental-search query for the focused panel, if in search mode.
    pub search: Option<String>,
    /// Active "go to directory" input buffer, if in goto mode.
    pub goto: Option<String>,
    pub show_help: bool,
    pub quit: bool,

    // Wall-clock elapsed tracking (robust across pause/seek without needing the
    // file's PPQ division). Used only for the time readout, not the progress bar.
    play_started: Option<Instant>,
    accumulated_secs: f64,
}

impl App {
    pub fn new(midi_dir: Location, sf2_dir: Location, driver: Option<&str>) -> Result<App, String> {
        let (mut synth, warn) = Synth::new(driver)?;
        let volume = 60u8;
        synth.set_gain(volume_to_gain(volume));

        Ok(App {
            // Only the MIDI panel browses into archives; SoundFonts come from
            // the filesystem only.
            midi: Browser::new_at(midi_dir, MIDI_EXTS, true, true),
            sf2: Browser::new_at(sf2_dir, SF2_EXTS, false, false),
            active: Panel::Midi,
            synth,
            state: PlayState::Stopped,
            now_playing: None,
            soundfont: None,
            last_played: None,
            cur_info: None,
            history: History::default(),
            favs: Favourites::default(),
            pending: None,
            overlay: None,
            play_source: PlaySource::Directory,
            play_temp: None,
            sf_temp: None,
            volume,
            repeat: false,
            next_mode: true,
            message: warn,
            search: None,
            goto: None,
            show_help: false,
            quit: false,
            play_started: None,
            accumulated_secs: 0.0,
        })
    }

    pub fn active_browser(&mut self) -> &mut Browser {
        match self.active {
            Panel::Midi => &mut self.midi,
            Panel::Sf2 => &mut self.sf2,
        }
    }

    pub fn toggle_panel(&mut self) {
        self.active = match self.active {
            Panel::Midi => Panel::Sf2,
            Panel::Sf2 => Panel::Midi,
        };
    }

    /// Jump both panels' cursors to what they are currently playing/loading:
    /// the playing MIDI file in the MIDI panel and the loaded SoundFont in the
    /// SoundFont panel. Each panel navigates into the right directory or archive
    /// first. Panels with nothing to reveal are left untouched.
    pub fn goto_current(&mut self) {
        let midi = self.now_playing.clone();
        let sf2 = self.soundfont.clone();
        if let Some(loc) = &midi {
            self.midi.reveal(loc);
        }
        if let Some(loc) = &sf2 {
            self.sf2.reveal(loc);
        }
        self.message = match (midi.is_some(), sf2.is_some()) {
            (false, false) => Some("Nothing playing or loaded".into()),
            _ => None,
        };
    }

    /// Enter directory or act on the selected file depending on the panel.
    pub fn activate_selection(&mut self) {
        let is_dir = self
            .active_browser()
            .selected()
            .map(|e| e.is_dir)
            .unwrap_or(false);
        if is_dir {
            self.active_browser().enter_dir();
            return;
        }
        let loc = match self.active_browser().selected() {
            Some(e) => e.loc.clone(),
            None => return,
        };
        match self.active {
            // Loading a font by hand is the A/B move; it does not leave the
            // favourites playlist, whose next entry brings its own font.
            Panel::Sf2 => self.load_soundfont(loc),
            // Playing from the panel hands the queue back to the directory.
            Panel::Midi => {
                self.play_source = PlaySource::Directory;
                self.play_path(loc);
            }
        }
    }

    // --- incremental filter/search (the `/` key) ------------------------------

    /// Push the active panel's filter to match the current search buffer.
    fn apply_search(&mut self) {
        let q = self.search.clone().unwrap_or_default();
        self.active_browser().set_filter(&q);
    }

    pub fn search_push(&mut self, c: char) {
        if let Some(q) = self.search.as_mut() {
            q.push(c);
        }
        self.apply_search();
    }

    pub fn search_backspace(&mut self) {
        if let Some(q) = self.search.as_mut() {
            q.pop();
        }
        self.apply_search();
    }

    /// Leave search mode, clearing the filter and restoring the full listing.
    pub fn search_cancel(&mut self) {
        self.search = None;
        self.active_browser().set_filter("");
    }

    /// Accept the highlighted match: act on it (play / load / enter dir), then
    /// leave search mode and restore the full listing (cursor stays on the item
    /// when it is still present).
    pub fn search_accept(&mut self) {
        self.search = None;
        self.activate_selection();
        self.active_browser().set_filter("");
    }

    pub fn load_soundfont(&mut self, loc: Location) {
        self.load_font(loc, true);
    }

    /// Load the SoundFont remembered from the last session without writing a
    /// history entry: the user did not pick it this time, and an automatic
    /// restore must not push it to the top of the history on every launch.
    pub fn restore_soundfont(&mut self, loc: Location) {
        self.load_font(loc, false);
    }

    /// Load `loc` into the synth, remembering the combination when `remember`.
    fn load_font(&mut self, loc: Location, remember: bool) {
        // Archive members are extracted to a temp file first, since the FFI
        // loads SoundFonts by filename only.
        let (path, guard) = match vfs::resolve_to_file(&loc) {
            Ok(r) => r,
            Err(e) => {
                self.message = Some(e);
                return;
            }
        };
        match self.synth.load_soundfont(&path) {
            Ok(()) => {
                let name = loc.file_name();
                // Swapping the font under a playing track is the A/B move worth
                // remembering as a combination; it starts a fresh dwell. With
                // nothing playing there is no listening time to wait for, so the
                // font is remembered on its own straight away.
                self.commit_history();
                if !remember {
                    // Nothing to remember, but the combination changed: drop any
                    // pending entry so it is not credited to the new font.
                    self.pending = None;
                } else if self.state != PlayState::Stopped && self.now_playing.is_some() {
                    self.pending = Some(Pending {
                        midi: self.now_playing.clone(),
                        soundfont: Some(loc.clone()),
                        at_secs: self.elapsed_secs(),
                        count: true,
                    });
                } else {
                    self.history
                        .record(None, Some(loc.clone()), history::now_secs(), true);
                    self.history.save();
                }
                self.soundfont = Some(loc);
                self.sf_temp = guard;
                self.message = Some(format!("SoundFont loaded: {name}"));
                self.save_state();
            }
            Err(e) => self.message = Some(e),
        }
    }

    /// Persist the current directories, the last-played MIDI file and the loaded
    /// SoundFont for next launch. Directories and the played file keep their full
    /// location, so an archive (or a file inside one) is restored as such.
    pub fn save_state(&self) {
        crate::state::save(&crate::state::State {
            midi_dir: Some(self.midi.location()),
            midi_file: self.last_played.clone(),
            sf2_dir: Some(self.sf2.location()),
            soundfont: self.soundfont.clone(),
        });
    }

    pub fn play_path(&mut self, loc: Location) {
        self.play_track(loc, true);
    }

    /// Play `loc` and start remembering it. `count_play` is false when repeat
    /// mode is looping the same track again: the history entry's time is
    /// refreshed, but one long loop is still one listening session.
    fn play_track(&mut self, loc: Location, count_play: bool) {
        if !self.synth.has_soundfont() {
            self.message =
                Some("No SoundFont loaded — pick one in the right panel (Tab, then Enter)".into());
            return;
        }
        // Archive members are extracted to a temp file first, since the FFI
        // plays MIDI by filename only. The guard is kept alive past `play`
        // because fluidsynth loads the file lazily on its audio thread.
        let (path, guard) = match vfs::resolve_to_file(&loc) {
            Ok(r) => r,
            Err(e) => {
                self.message = Some(e);
                return;
            }
        };
        match self.synth.play(&path) {
            Ok(()) => {
                // Commit what was playing while the playback clock still belongs
                // to it, then restart the clock for this track.
                self.commit_history();
                self.message = None;
                self.cur_info = midi::parse(&path);
                self.now_playing = Some(loc.clone());
                self.last_played = Some(loc.clone());
                self.play_temp = guard;
                self.state = PlayState::Playing;
                self.play_started = Some(Instant::now());
                self.accumulated_secs = 0.0;
                self.pending = Some(Pending {
                    midi: Some(loc),
                    soundfont: self.soundfont.clone(),
                    at_secs: 0.0,
                    count: count_play,
                });
            }
            Err(e) => self.message = Some(e),
        }
    }

    pub fn toggle_pause(&mut self) {
        match self.state {
            PlayState::Playing => {
                self.synth.pause();
                self.accumulate();
                self.play_started = None;
                self.state = PlayState::Paused;
            }
            PlayState::Paused => {
                self.synth.resume();
                self.play_started = Some(Instant::now());
                self.state = PlayState::Playing;
            }
            PlayState::Stopped => {}
        }
    }

    pub fn stop(&mut self) {
        if self.state == PlayState::Stopped {
            return;
        }
        self.commit_history();
        self.pending = None;
        self.synth.stop();
        self.state = PlayState::Stopped;
        self.play_started = None;
        self.accumulated_secs = 0.0;
    }

    /// Seek by a number of seconds (positive or negative).
    pub fn seek_seconds(&mut self, secs: i32) {
        if self.state == PlayState::Stopped {
            return;
        }
        let tps = self.ticks_per_second().unwrap_or(0.0);
        let delta = if tps > 0.0 {
            (secs as f64 * tps) as i32
        } else if let Some((_, total)) = self.synth.position() {
            // Fall back to ~2% of the song per "second-ish" step.
            ((secs as f64) * (total as f64) / 50.0) as i32
        } else {
            0
        };
        self.synth.seek_ticks(delta);
        // Nudge the wall-clock estimate so the readout tracks the seek.
        self.accumulate();
        self.accumulated_secs = (self.accumulated_secs + secs as f64).max(0.0);
        if self.state == PlayState::Playing {
            self.play_started = Some(Instant::now());
        }
    }

    pub fn volume_delta(&mut self, delta: i32) {
        let v = (self.volume as i32 + delta).clamp(0, 100) as u8;
        self.volume = v;
        self.synth.set_gain(volume_to_gain(v));
    }

    pub fn set_volume(&mut self, v: u8) {
        self.volume = v.min(100);
        self.synth.set_gain(volume_to_gain(self.volume));
    }

    pub fn toggle_repeat(&mut self) {
        self.repeat = !self.repeat;
        self.message = Some(format!("Repeat: {}", on_off(self.repeat)));
    }

    pub fn toggle_next_mode(&mut self) {
        self.next_mode = !self.next_mode;
        self.message = Some(format!("Next: {}", on_off(self.next_mode)));
    }

    pub fn toggle_hidden(&mut self) {
        let show = !self.active_browser().show_hidden;
        // Keep both panels consistent.
        self.midi.show_hidden = show;
        self.sf2.show_hidden = show;
        self.midi.refresh();
        self.sf2.refresh();
    }

    /// Called once per UI tick: on end-of-song, apply the next/repeat modes.
    ///
    /// | next | repeat | on finish                              |
    /// |------|--------|----------------------------------------|
    /// | off  | off    | stop                                   |
    /// | off  | on     | replay current track (loop track)      |
    /// | on   | off    | play next; stop after the last         |
    /// | on   | on     | play next; wrap to the first           |
    ///
    /// "Next" means the next file in the playing file's directory, or the next
    /// starred pair when the favourites started this track — see
    /// [`PlaySource`]. The table itself is the same either way.
    pub fn tick(&mut self) {
        // A combination becomes history as soon as it has been heard long
        // enough, rather than at the end of the track, so it survives a crash
        // and shows up in the overlay while it is still playing.
        self.commit_history();
        if self.state != PlayState::Playing {
            return;
        }
        // Use the player's DONE status, not the tick counter: fluidsynth can
        // report completion at a tick below (or above) the nominal total.
        if !self.synth.is_finished() {
            return;
        }

        let cur = match self.now_playing.clone() {
            Some(p) => p,
            None => {
                self.stop();
                return;
            }
        };

        if self.next_mode {
            match self.next_in_source(&cur) {
                Some(step) => self.play_step(step),
                None => self.stop(),
            }
        } else if self.repeat {
            self.play_track(cur, false); // loop the current track
        } else {
            self.stop();
        }
    }

    /// What follows `cur`, according to the current play source. `None` means
    /// there is nothing left to play and the player should stop.
    fn next_in_source(&mut self, cur: &Location) -> Option<Step> {
        match self.play_source.clone() {
            PlaySource::Directory => {
                let next = match self.midi.neighbour_file(cur, true) {
                    Some(n) => Some(n),
                    // End of directory: loop back to the first file of the
                    // playing file's directory (not wherever the user is now
                    // browsing).
                    None if self.repeat => self.midi.first_file_of(cur),
                    None => None,
                };
                next.map(|midi| Step {
                    midi,
                    soundfont: None,
                    fav_idx: None,
                })
            }
            PlaySource::Favourites { key, idx } => self.next_favourite(&key, idx),
        }
    }

    /// Walk forward through the favourites from the entry that just played,
    /// skipping any whose track or SoundFont has gone. The scan is capped at
    /// one pass over the list, so a playlist whose files have all disappeared
    /// stops rather than spinning.
    fn next_favourite(&self, key: &(Location, Location), idx: usize) -> Option<Step> {
        let n = self.favs.len();
        if n == 0 {
            return None;
        }
        // The entry that just played may have been un-starred while it played.
        // If it is gone, carry on with whatever moved into its slot instead of
        // stepping over it; otherwise its current position is authoritative,
        // so a reorder mid-playlist is followed rather than fought.
        let (at, first) = match self.favs.position_of(&key.0, &key.1) {
            Some(p) => (p, 1),
            None => (idx, 0),
        };
        for offset in first..first + n {
            let i = match at + offset {
                i if i < n => i,
                i if self.repeat => i % n,
                _ => return None,
            };
            let fav = self.favs.get(i)?;
            if fav.midi.exists() && fav.soundfont.exists() {
                return Some(Step {
                    midi: fav.midi.clone(),
                    soundfont: Some(fav.soundfont.clone()),
                    fav_idx: Some(i),
                });
            }
        }
        None
    }

    /// Play what [`Self::next_in_source`] resolved, loading the step's own
    /// SoundFont first when it brought one.
    fn play_step(&mut self, step: Step) {
        if let (Some(sf), Some(i)) = (step.soundfont.clone(), step.fav_idx) {
            // Advancing the queue is not a choice the user made, so the font is
            // loaded without a history record: the play itself is recorded, and
            // a font-only row (or one pairing the finished track with the new
            // font) would be noise.
            if self.soundfont.as_ref() != Some(&sf) {
                self.load_font(sf.clone(), false);
            }
            self.play_source = PlaySource::Favourites {
                key: (step.midi.clone(), sf),
                idx: i,
            };
        }
        self.play_track(step.midi, true);
    }

    /// Progress fraction 0.0..=1.0 from the player's tick counters.
    pub fn progress(&self) -> f64 {
        match self.synth.position() {
            Some((cur, total)) if total > 0 => (cur as f64 / total as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    /// (elapsed_secs, total_secs). Total is exact from the parsed file when
    /// available, otherwise estimated from elapsed time and progress.
    pub fn times(&self) -> (f64, f64) {
        let elapsed = self.elapsed_secs();
        let total = match self.cur_info {
            Some(info) if info.duration_secs > 0.0 => info.duration_secs,
            _ => {
                let frac = self.progress();
                if frac > 0.001 {
                    elapsed / frac
                } else {
                    0.0
                }
            }
        };
        (elapsed, total)
    }

    /// Current musical position as (bar, beat), both 1-based, if computable.
    pub fn bar_beat(&self) -> Option<(u32, u32)> {
        let info = self.cur_info?;
        let (cur, _) = self.synth.position()?;
        bar_beat_at(cur, &info)
    }

    /// Live playback tempo in BPM, if available.
    pub fn bpm(&self) -> Option<i32> {
        self.synth.bpm()
    }

    /// Time signature as (numerator, denominator), if known.
    pub fn time_signature(&self) -> Option<(u8, u8)> {
        self.cur_info.map(|i| (i.ts_num, i.ts_den))
    }

    fn elapsed_secs(&self) -> f64 {
        let live = self
            .play_started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        self.accumulated_secs + live
    }

    fn accumulate(&mut self) {
        if let Some(t) = self.play_started {
            self.accumulated_secs += t.elapsed().as_secs_f64();
        }
    }

    // --- playing history (the `R` key) ----------------------------------------

    /// Write the pending combination to the history once it has actually been
    /// heard for [`MIN_LISTEN_SECS`] of playback. Called at every transition and
    /// on each UI tick; a combination that never reached the threshold is simply
    /// dropped when the next one starts.
    fn commit_history(&mut self) {
        let due = match &self.pending {
            Some(p) => self.elapsed_secs() - p.at_secs >= MIN_LISTEN_SECS,
            None => false,
        };
        if !due {
            return;
        }
        if let Some(p) = self.pending.take() {
            self.history
                .record(p.midi, p.soundfont, history::now_secs(), p.count);
            self.history.save();
        }
    }

    /// Commit whatever is still pending and flush both stores — called on exit.
    /// The favourites save on every change, so this only catches a write that
    /// failed earlier.
    pub fn finish(&mut self) {
        self.commit_history();
        self.history.save();
        self.favs.save();
    }

    // --- favourites (the `f` key) ---------------------------------------------

    /// Star, or un-star, the pair being heard: the loaded track through the
    /// loaded SoundFont. A favourite is always a pair — the judgement worth
    /// keeping is "this tune through this font" — so both are required. The
    /// cursor is deliberately not consulted: in the ordinary flow it is on the
    /// playing track anyway, and when it is not, what you are hearing is what
    /// you mean. `stop()` leaves `now_playing` set, so this still works on the
    /// track that just ended.
    pub fn toggle_favourite(&mut self) {
        let (midi, sf) = match (self.now_playing.clone(), self.soundfont.clone()) {
            (Some(m), Some(s)) => (m, s),
            _ => {
                self.message =
                    Some("Nothing playing — a favourite is a track + SoundFont pair".into());
                return;
            }
        };
        self.star(midi, sf);
    }

    /// Toggle the star on one pair and report it. A star is an explicit act, so
    /// unlike the history it is stored at once, with no listening threshold.
    fn star(&mut self, midi: Location, soundfont: Location) {
        let name = midi.file_name();
        let font = soundfont.file_name();
        let added = self.favs.toggle(midi, soundfont, history::now_secs());
        self.favs.save();
        self.message = Some(if added {
            format!("★ {name} + {font}")
        } else {
            format!("Unstarred: {name} + {font}")
        });
    }

    /// The star to draw beside a browser entry: solid when this exact pair is
    /// starred, hollow when the item is starred in some other pairing, blank
    /// otherwise. The hollow star is what makes a pairs-only list readable
    /// while browsing — swap the font and the solid stars move, showing at a
    /// glance which combinations are already starred and which are new ground.
    pub fn star_for(&self, panel: Panel, loc: &Location) -> &'static str {
        let (exact, any) = match panel {
            Panel::Midi => (
                self.soundfont
                    .as_ref()
                    .map(|sf| self.favs.is_pair(loc, sf))
                    .unwrap_or(false),
                self.favs.has_track(loc),
            ),
            Panel::Sf2 => (
                self.now_playing
                    .as_ref()
                    .map(|m| self.favs.is_pair(m, loc))
                    .unwrap_or(false),
                self.favs.has_font(loc),
            ),
        };
        match (exact, any) {
            (true, _) => "★",
            (false, true) => "☆",
            _ => " ",
        }
    }

    /// True when the pair being heard is starred, for the player-bar badge.
    pub fn current_is_favourite(&self) -> bool {
        match (&self.now_playing, &self.soundfont) {
            (Some(m), Some(sf)) => self.favs.is_pair(m, sf),
            _ => false,
        }
    }

    /// (position, length) in the favourites playlist, when it is what is
    /// playing. `None` while the queue is the directory.
    pub fn playlist_position(&self) -> Option<(usize, usize)> {
        match &self.play_source {
            PlaySource::Directory => None,
            PlaySource::Favourites { key, idx } => {
                // The list can be emptied, or shortened, while it is the queue.
                let n = self.favs.len();
                if n == 0 {
                    return None;
                }
                let at = self
                    .favs
                    .position_of(&key.0, &key.1)
                    .unwrap_or(*idx)
                    .min(n - 1);
                Some((at + 1, n))
            }
        }
    }

    // --- the history / favourites overlay (the `R` and `F` keys) --------------

    pub fn history_open(&mut self) {
        self.overlay_open(Source::History);
    }

    pub fn favourites_open(&mut self) {
        self.overlay_open(Source::Favourites);
    }

    fn overlay_open(&mut self, source: Source) {
        // The overlay and the panel filter are exclusive modes: leaving search
        // restores the full listing behind the overlay.
        if self.search.is_some() {
            self.search_cancel();
        }
        let view = View::Pairs;
        let rows = self.overlay_rows(source, view, "");
        let mut state = ListState::default();
        state.select((!rows.is_empty()).then_some(0));
        self.message = match source {
            Source::History if self.history.is_empty() => Some("History is empty".to_string()),
            Source::Favourites if self.favs.is_empty() => {
                Some("No favourites yet — press f while a track is playing".to_string())
            }
            _ => None,
        };
        self.overlay = Some(Overlay {
            source,
            view,
            state,
            rows,
            filter: String::new(),
            filtering: false,
            confirm_clear: false,
        });
    }

    fn overlay_rows(&self, source: Source, view: View, filter: &str) -> Vec<Row> {
        match source {
            Source::History => self.history.rows(view, filter),
            Source::Favourites => self.favs.rows(filter, &self.history),
        }
    }

    pub fn overlay_close(&mut self) {
        self.overlay = None;
    }

    /// True while the overlay's `/` filter is capturing keystrokes.
    pub fn overlay_filtering(&self) -> bool {
        self.overlay.as_ref().map(|o| o.filtering).unwrap_or(false)
    }

    /// The store the open overlay is showing, if any.
    pub fn overlay_source(&self) -> Option<Source> {
        self.overlay.as_ref().map(|o| o.source)
    }

    /// Rebuild the visible rows after a view, filter or content change. The
    /// cursor is kept (clamped) when the rows still describe the same list.
    fn overlay_rebuild(&mut self, keep_cursor: bool) {
        let (source, view, filter, cur) = match &self.overlay {
            Some(o) => (
                o.source,
                o.view,
                o.filter.clone(),
                o.state.selected().unwrap_or(0),
            ),
            None => return,
        };
        let rows = self.overlay_rows(source, view, &filter);
        if let Some(o) = self.overlay.as_mut() {
            let idx = match (rows.is_empty(), keep_cursor) {
                (true, _) => None,
                (false, true) => Some(cur.min(rows.len() - 1)),
                (false, false) => Some(0),
            };
            o.rows = rows;
            o.state.select(idx);
        }
    }

    /// Cycle the history overlay between the combination, per-track and
    /// per-SoundFont views of the same log. The favourites have no views.
    pub fn overlay_cycle_view(&mut self) {
        match self.overlay.as_mut() {
            Some(o) if o.source == Source::History => o.view = o.view.next(),
            _ => return,
        }
        self.overlay_rebuild(false);
    }

    pub fn overlay_start_filter(&mut self) {
        if let Some(o) = self.overlay.as_mut() {
            o.filtering = true;
            o.filter.clear();
        }
        self.overlay_rebuild(false);
    }

    pub fn overlay_filter_push(&mut self, c: char) {
        if let Some(o) = self.overlay.as_mut() {
            o.filter.push(c);
        }
        self.overlay_rebuild(false);
    }

    pub fn overlay_filter_backspace(&mut self) {
        if let Some(o) = self.overlay.as_mut() {
            o.filter.pop();
        }
        self.overlay_rebuild(false);
    }

    pub fn overlay_filter_cancel(&mut self) {
        if let Some(o) = self.overlay.as_mut() {
            o.filtering = false;
            o.filter.clear();
        }
        self.overlay_rebuild(false);
    }

    /// Play the selected row. From the history that means hearing the entry
    /// again exactly as it was, and the queue reverts to the directory; from
    /// the favourites it starts the playlist at that entry.
    pub fn overlay_activate(&mut self) {
        let (source, row) = match self.overlay.as_ref() {
            Some(o) => match o.selected() {
                Some(r) => (o.source, r.clone()),
                None => return,
            },
            None => return,
        };
        self.overlay = None;
        match source {
            Source::History => {
                self.play_source = PlaySource::Directory;
                self.play_combination(row.midi, row.soundfont);
            }
            Source::Favourites => {
                if let Some(&idx) = row.idxs.first() {
                    self.play_favourite_at(idx);
                }
            }
        }
    }

    /// Start (or restart) the favourites playlist at `idx`.
    fn play_favourite_at(&mut self, idx: usize) {
        let fav = match self.favs.get(idx) {
            Some(f) => f.clone(),
            None => return,
        };
        if self.play_combination(Some(fav.midi.clone()), Some(fav.soundfont.clone())) {
            self.play_source = PlaySource::Favourites {
                key: (fav.midi, fav.soundfont),
                idx,
            };
        }
    }

    /// Play a (track, SoundFont) combination the way the overlays do: its
    /// SoundFont is loaded first (unless already loaded), then its track. Both
    /// are locations, so a file inside a zip archive is extracted and played
    /// just like it was the first time. Returns false when a side has gone,
    /// having left the panels and the player untouched.
    fn play_combination(&mut self, midi: Option<Location>, soundfont: Option<Location>) -> bool {
        // Check both sides first, so an entry that can no longer be played
        // leaves the panels exactly as they were.
        if let Some(sf) = &soundfont {
            if !sf.exists() {
                self.message = Some(format!("SoundFont is gone: {}", sf.display()));
                return false;
            }
        }
        if let Some(midi) = &midi {
            if !midi.exists() {
                self.message = Some(format!("File is gone: {}", midi.display()));
                return false;
            }
        }

        // Follow the entry in the panels before playing it, so the cursor ends
        // up on the file itself — inside the archive when that is where it
        // lives, not parked on the `.zip`.
        if let Some(midi) = &midi {
            self.midi.reveal(midi);
        }
        if let Some(sf) = &soundfont {
            self.sf2.reveal(sf);
        }

        if let Some(sf) = soundfont {
            if self.soundfont.as_ref() != Some(&sf) {
                self.load_soundfont(sf);
            }
        }
        if let Some(midi) = midi {
            self.play_track(midi, true);
        }
        true
    }

    /// Point both panels at the selected row without playing it, the way `G`
    /// does for what is currently playing.
    pub fn overlay_reveal(&mut self) {
        let row = match self.overlay.as_ref().and_then(|o| o.selected()) {
            Some(r) => r.clone(),
            None => return,
        };
        self.overlay = None;
        if let Some(m) = &row.midi {
            self.midi.reveal(m);
        }
        if let Some(sf) = &row.soundfont {
            self.sf2.reveal(sf);
        }
    }

    /// Drop the selected row: forget it from the history (in a grouped view,
    /// every entry behind it), or take the star off a favourite.
    pub fn overlay_delete(&mut self) {
        let (source, idxs) = match self.overlay.as_ref() {
            Some(o) => match o.selected() {
                Some(r) => (o.source, r.idxs.clone()),
                None => return,
            },
            None => return,
        };
        match source {
            Source::History => {
                self.history.forget(&idxs);
                self.history.save();
            }
            Source::Favourites => {
                if let Some(&i) = idxs.first() {
                    self.favs.remove(i);
                    self.favs.save();
                }
            }
        }
        self.overlay_rebuild(true);
    }

    /// `f` inside an overlay: star the selected history row, or un-star the
    /// selected favourite (where it is the same gesture as `d`).
    pub fn overlay_toggle_favourite(&mut self) {
        let (source, row) = match self.overlay.as_ref() {
            Some(o) => match o.selected() {
                Some(r) => (o.source, r.clone()),
                None => return,
            },
            None => return,
        };
        if source == Source::Favourites {
            self.overlay_delete();
            return;
        }
        match (row.midi, row.soundfont) {
            (Some(m), Some(sf)) => {
                self.star(m, sf);
                self.overlay_rebuild(true);
            }
            // A SoundFont loaded with nothing playing is half a pair.
            _ => {
                self.message =
                    Some("Not a pair — a favourite needs both a track and a SoundFont".into())
            }
        }
    }

    /// Move the selected favourite one place along the playlist, the cursor
    /// following it. Under a filter the row swaps with its visible neighbour,
    /// so the move is always the one on screen.
    pub fn overlay_move(&mut self, down: bool) {
        let (i, rows) = match self.overlay.as_ref() {
            Some(o) if o.source == Source::Favourites => {
                (o.state.selected().unwrap_or(0), o.rows.clone())
            }
            _ => return,
        };
        let j = match down {
            true => i + 1,
            false => match i.checked_sub(1) {
                Some(j) => j,
                None => return,
            },
        };
        let (a, b) = match (rows.get(i), rows.get(j)) {
            (Some(a), Some(b)) => (a.idxs[0], b.idxs[0]),
            _ => return,
        };
        self.favs.swap(a, b);
        self.favs.save();
        self.overlay_rebuild(true);
        if let Some(o) = self.overlay.as_mut() {
            o.state.select(Some(j.min(o.rows.len().saturating_sub(1))));
        }
    }

    /// Clear the whole history. The first press only asks; the second one does
    /// it, since there is no undo. The favourites have no such key: every one
    /// of them was starred by hand, so erasing the lot has no honest use.
    pub fn history_clear(&mut self) {
        if self.overlay_source() != Some(Source::History) {
            return;
        }
        if !self
            .overlay
            .as_ref()
            .map(|o| o.confirm_clear)
            .unwrap_or(false)
        {
            if let Some(o) = self.overlay.as_mut() {
                o.confirm_clear = true;
            }
            self.message = Some("Press D again to erase the whole history".into());
            return;
        }
        self.history.clear();
        self.history.save();
        if let Some(o) = self.overlay.as_mut() {
            o.confirm_clear = false;
        }
        self.overlay_rebuild(false);
        self.message = Some("History erased".into());
    }

    /// Any other keypress takes back the pending "erase everything" question.
    pub fn overlay_cancel_confirm(&mut self) {
        if let Some(o) = self.overlay.as_mut() {
            if o.confirm_clear {
                o.confirm_clear = false;
                self.message = None;
            }
        }
    }

    // --- "go to directory" prompt (the `i` key) --------------------------------

    /// Open the GO prompt, pre-filled with the active panel's directory. The
    /// prompt navigates the filesystem only, so an archive resolves to the
    /// directory that holds it.
    pub fn start_goto(&mut self) {
        let mut d = self.active_browser().fs_dir().to_string_lossy().to_string();
        if !d.ends_with('/') {
            d.push('/');
        }
        self.goto = Some(d);
    }

    pub fn goto_push(&mut self, c: char) {
        if let Some(g) = self.goto.as_mut() {
            g.push(c);
        }
    }

    pub fn goto_backspace(&mut self) {
        if let Some(g) = self.goto.as_mut() {
            g.pop();
        }
    }

    /// Delete the last path component (back to the previous slash).
    pub fn goto_delete_component(&mut self) {
        if let Some(g) = self.goto.as_mut() {
            *g = delete_path_component(g);
        }
    }

    pub fn goto_cancel(&mut self) {
        self.goto = None;
    }

    /// Tab-complete the path in the GO buffer using rustyline's filesystem
    /// completer (handles the directory scan, matching and common-prefix logic).
    pub fn goto_complete(&mut self) {
        let input = match self.goto.clone() {
            Some(i) => i,
            None => return,
        };
        let completer = FilenameCompleter::new();
        let (start, candidates) = match completer.complete_path(&input, input.len()) {
            Ok(c) => c,
            Err(_) => return,
        };
        match candidates.len() {
            0 => {}
            1 => {
                self.goto = Some(format!(
                    "{}{}",
                    &input[..start],
                    candidates[0].replacement()
                ));
            }
            n => {
                if let Some(lcp) = longest_common_prefix(&candidates) {
                    self.goto = Some(format!("{}{}", &input[..start], lcp));
                }
                self.message = Some(format!("{n} matches"));
            }
        }
    }

    /// Navigate the active panel to the entered directory.
    pub fn goto_submit(&mut self) {
        let input = match self.goto.take() {
            Some(i) => i,
            None => return,
        };
        let path = PathBuf::from(expand_tilde(input.trim()));
        if path.is_dir() {
            self.message = None;
            self.active_browser().set_dir(path);
        } else {
            self.message = Some(format!("Not a directory: {}", input.trim()));
        }
    }

    fn ticks_per_second(&self) -> Option<f64> {
        let (cur, _) = self.synth.position()?;
        let elapsed = self.elapsed_secs();
        if elapsed > 0.5 && cur > 0 {
            Some(cur as f64 / elapsed)
        } else {
            None
        }
    }
}

fn volume_to_gain(v: u8) -> f32 {
    // Map 0..100% to a comfortable 0.0..0.8 gain (fluidsynth default is 0.2).
    (v as f32 / 100.0) * 0.8
}

fn on_off(b: bool) -> &'static str {
    if b {
        "ON"
    } else {
        "OFF"
    }
}

fn expand_tilde(s: &str) -> String {
    let home = || std::env::var("HOME").unwrap_or_default();
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{}/{}", home(), rest)
    } else {
        s.to_string()
    }
}

/// Drop the last path component of `s`, keeping the trailing slash (for the GO
/// prompt's Alt+Backspace). "/a/b/c" -> "/a/b/", "/a/b/" -> "/a/".
fn delete_path_component(s: &str) -> String {
    let mut s = s.to_string();
    if s.ends_with('/') {
        s.pop();
    }
    match s.rfind('/') {
        Some(i) => s.truncate(i + 1),
        None => s.clear(),
    }
    s
}

/// Pure (bar, beat) computation, both 1-based. None for SMPTE/unknown division.
fn bar_beat_at(tick: i32, info: &MidiInfo) -> Option<(u32, u32)> {
    if info.division == 0 {
        return None;
    }
    let cur = tick.max(0) as u64;
    let div = info.division as u64;
    // Ticks per beat, where a "beat" is one denominator note.
    let ticks_per_beat = (div * 4 / info.ts_den.max(1) as u64).max(1);
    let beats_per_bar = info.ts_num.max(1) as u64;
    let ticks_per_bar = ticks_per_beat * beats_per_bar;
    let bar = cur / ticks_per_bar + 1;
    let beat = (cur % ticks_per_bar) / ticks_per_beat + 1;
    Some((bar as u32, beat as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_to_gain_maps_range() {
        assert_eq!(volume_to_gain(0), 0.0);
        assert!((volume_to_gain(100) - 0.8).abs() < 1e-6);
        assert!((volume_to_gain(50) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn delete_path_component_steps_up() {
        assert_eq!(delete_path_component("/a/b/c"), "/a/b/");
        assert_eq!(delete_path_component("/a/b/"), "/a/");
        assert_eq!(delete_path_component("/a/"), "/");
        assert_eq!(delete_path_component("/"), "");
        assert_eq!(delete_path_component("relative"), "");
    }

    #[test]
    fn expand_tilde_uses_home() {
        // Independent of the environment: a plain path is unchanged.
        assert_eq!(expand_tilde("/etc/passwd"), "/etc/passwd");
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(expand_tilde("~/x"), format!("{home}/x"));
            assert_eq!(expand_tilde("~"), home);
        }
    }

    #[test]
    fn bar_beat_4_4() {
        // 480 PPQ, 4/4: ticks/beat=480, ticks/bar=1920.
        let info = MidiInfo {
            division: 480,
            duration_secs: 0.0,
            ts_num: 4,
            ts_den: 4,
        };
        assert_eq!(bar_beat_at(0, &info), Some((1, 1)));
        assert_eq!(bar_beat_at(480, &info), Some((1, 2)));
        assert_eq!(bar_beat_at(1920, &info), Some((2, 1)));
        assert_eq!(bar_beat_at(1920 + 960, &info), Some((2, 3)));
    }

    /// An app rooted at two fresh temp directories, with an in-memory history.
    fn test_app() -> (App, tempfile::TempDir, tempfile::TempDir) {
        let midi_dir = tempfile::tempdir().unwrap();
        let sf_dir = tempfile::tempdir().unwrap();
        let app = App::new(
            Location::Fs(midi_dir.path().to_path_buf()),
            Location::Fs(sf_dir.path().to_path_buf()),
            None,
        )
        .expect("app init");
        (app, midi_dir, sf_dir)
    }

    fn loc(p: &str) -> Option<Location> {
        Some(Location::Fs(PathBuf::from(p)))
    }

    #[test]
    fn history_waits_until_a_track_has_actually_been_heard() {
        let (mut app, _m, _s) = test_app();
        app.pending = Some(Pending {
            midi: loc("/m/a.mid"),
            soundfont: loc("/s/one.sf2"),
            at_secs: 0.0,
            count: true,
        });

        // Skipped through after a couple of seconds: not worth remembering.
        app.accumulated_secs = 2.0;
        app.commit_history();
        assert!(app.history.is_empty());

        // Listened to past the threshold: remembered, and only once.
        app.accumulated_secs = MIN_LISTEN_SECS + 0.5;
        app.commit_history();
        app.commit_history();
        assert_eq!(app.history.len(), 1);
        assert!(app.pending.is_none());
    }

    #[test]
    fn history_overlay_lists_filters_and_forgets() {
        let (mut app, _m, _s) = test_app();
        app.history
            .record(loc("/m/a.mid"), loc("/s/one.sf2"), 100, true);
        app.history
            .record(loc("/m/b.mid"), loc("/s/one.sf2"), 200, true);

        app.history_open();
        let hist = app.overlay.as_ref().expect("overlay open");
        assert_eq!(hist.rows.len(), 2);
        assert_eq!(hist.view, View::Pairs);
        // Newest first, cursor on the top row.
        assert_eq!(hist.selected().unwrap().midi, loc("/m/b.mid"));

        // Tab walks the three views of the same log.
        app.overlay_cycle_view();
        assert_eq!(app.overlay.as_ref().unwrap().view, View::Tracks);
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 2);
        app.overlay_cycle_view();
        assert_eq!(app.overlay.as_ref().unwrap().view, View::Fonts);
        assert_eq!(
            app.overlay.as_ref().unwrap().rows.len(),
            1,
            "both plays used the same SoundFont"
        );
        app.overlay_cycle_view();

        // `/` narrows the list; Esc restores it.
        app.overlay_start_filter();
        app.overlay_filter_push('a');
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 1);
        app.overlay_filter_cancel();
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 2);

        // `d` forgets the selected row and keeps the overlay usable.
        app.overlay_delete();
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 1);

        // `D` asks once, then erases.
        app.history_clear();
        assert_eq!(app.history.len(), 1, "the first D only asks");
        app.history_clear();
        assert!(app.history.is_empty());

        app.overlay_close();
        assert!(app.overlay.is_none());
    }

    #[test]
    fn restoring_a_vanished_file_reports_instead_of_playing() {
        let (mut app, _m, _s) = test_app();
        app.history
            .record(loc("/no/such/track.mid"), None, 100, true);
        app.history_open();
        app.overlay_activate();

        assert!(app.overlay.is_none(), "the overlay closes on activation");
        assert!(app.message.unwrap().contains("gone"));
        assert!(app.now_playing.is_none());
    }

    #[test]
    fn restoring_an_entry_moves_the_panels_onto_it() {
        use std::io::Write as _;

        let (mut app, midi_dir, sf_dir) = test_app();
        // A zip holding the track, plus a loose one beside it.
        let zip_path = midi_dir.path().join("songs.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("classics/canyon.mid", opts).unwrap();
            w.write_all(b"data").unwrap();
            w.finish().unwrap();
        }
        let loose = midi_dir.path().join("plain.mid");
        let font = sf_dir.path().join("piano.sf2");
        std::fs::write(&loose, b"x").unwrap();
        std::fs::write(&font, b"x").unwrap();
        app.midi.refresh();
        app.sf2.refresh();

        let member = Location::Zip {
            archive: zip_path.clone(),
            inner: "classics/canyon.mid".to_string(),
        };
        app.history.record(
            Some(member.clone()),
            Some(Location::Fs(font.clone())),
            100,
            true,
        );
        app.history_open();
        app.overlay_activate();

        // The MIDI panel is inside the archive, on the track itself — not left
        // sitting on the .zip — and the SoundFont panel is on its font.
        assert_eq!(
            app.midi.location(),
            Location::Zip {
                archive: zip_path,
                inner: "classics".to_string()
            }
        );
        assert_eq!(app.midi.selected().map(|e| e.loc.clone()), Some(member));
        assert_eq!(
            app.sf2.selected().map(|e| e.loc.clone()),
            Some(Location::Fs(font.clone()))
        );

        // A plain file moves the cursor the same way.
        app.history.record(
            Some(Location::Fs(loose.clone())),
            Some(Location::Fs(font)),
            200,
            true,
        );
        app.history_open();
        app.overlay_activate();
        assert_eq!(
            app.midi.location(),
            Location::Fs(midi_dir.path().to_path_buf())
        );
        assert_eq!(
            app.midi.selected().map(|e| e.loc.clone()),
            Some(Location::Fs(loose))
        );
    }

    #[test]
    fn history_reveal_points_both_panels_at_the_entry() {
        let (mut app, midi_dir, sf_dir) = test_app();
        let track = midi_dir.path().join("song.mid");
        let font = sf_dir.path().join("piano.sf2");
        std::fs::write(&track, b"x").unwrap();
        std::fs::write(&font, b"x").unwrap();
        app.midi.refresh();
        app.sf2.refresh();

        app.history.record(
            Some(Location::Fs(track.clone())),
            Some(Location::Fs(font.clone())),
            100,
            true,
        );
        app.history_open();
        app.overlay_reveal();

        assert!(app.overlay.is_none());
        assert_eq!(
            app.midi.selected().map(|e| e.loc.clone()),
            Some(Location::Fs(track))
        );
        assert_eq!(
            app.sf2.selected().map(|e| e.loc.clone()),
            Some(Location::Fs(font))
        );
    }

    #[test]
    fn goto_current_reveals_both_panels() {
        use crate::vfs::Location;
        use std::fs;

        let midi_dir = tempfile::tempdir().unwrap();
        let sf_dir = tempfile::tempdir().unwrap();
        // Put each "current" item in a subdirectory, so revealing it must
        // navigate the panel rather than just move the cursor.
        let msub = midi_dir.path().join("sub");
        let ssub = sf_dir.path().join("fonts");
        fs::create_dir(&msub).unwrap();
        fs::create_dir(&ssub).unwrap();
        fs::write(msub.join("song.mid"), b"x").unwrap();
        fs::write(ssub.join("piano.sf2"), b"x").unwrap();

        let mut app = App::new(
            Location::Fs(midi_dir.path().to_path_buf()),
            Location::Fs(sf_dir.path().to_path_buf()),
            None,
        )
        .expect("app init");

        let playing = Location::Fs(msub.join("song.mid"));
        let font = Location::Fs(ssub.join("piano.sf2"));
        app.now_playing = Some(playing.clone());
        app.soundfont = Some(font.clone());

        // A single G reveals the current item in *both* panels, regardless of
        // which one is active.
        app.goto_current();

        assert_eq!(app.midi.selected().map(|e| e.loc.clone()), Some(playing));
        assert_eq!(app.sf2.selected().map(|e| e.loc.clone()), Some(font));
        assert!(app.message.is_none());
    }

    #[test]
    fn bar_beat_3_4_and_smpte() {
        let info = MidiInfo {
            division: 480,
            duration_secs: 0.0,
            ts_num: 3,
            ts_den: 4,
        };
        // 3/4: a bar is 3 beats = 1440 ticks.
        assert_eq!(bar_beat_at(1440, &info), Some((2, 1)));
        assert_eq!(bar_beat_at(480, &info), Some((1, 2)));

        let smpte = MidiInfo {
            division: 0,
            ..info
        };
        assert_eq!(bar_beat_at(100, &smpte), None);
    }

    /// An app with `n` tracks (a.mid, b.mid, …) and one SoundFont on disk, each
    /// track starred against that font in name order.
    fn playlist_app(n: usize) -> (App, tempfile::TempDir, tempfile::TempDir, Location) {
        let (mut app, midi_dir, sf_dir) = test_app();
        let font_path = sf_dir.path().join("piano.sf2");
        std::fs::write(&font_path, b"x").unwrap();
        let font = Location::Fs(font_path);
        for i in 0..n {
            let name = format!("{}.mid", (b'a' + i as u8) as char);
            let path = midi_dir.path().join(&name);
            std::fs::write(&path, b"x").unwrap();
            app.favs
                .toggle(Location::Fs(path), font.clone(), 100 + i as u64);
        }
        app.midi.refresh();
        app.sf2.refresh();
        (app, midi_dir, sf_dir, font)
    }

    /// The (track, font) key of the favourite at `idx`.
    fn key_at(app: &App, idx: usize) -> (Location, Location) {
        let f = app.favs.get(idx).expect("a favourite");
        (f.midi.clone(), f.soundfont.clone())
    }

    #[test]
    fn starring_needs_both_a_track_and_a_soundfont() {
        let (mut app, _m, _s) = test_app();
        app.toggle_favourite();
        assert!(app.favs.is_empty());
        assert!(app.message.as_ref().unwrap().contains("Nothing playing"));

        app.now_playing = loc("/m/a.mid");
        app.soundfont = loc("/s/one.sf2");
        app.toggle_favourite();
        assert_eq!(app.favs.len(), 1);
        assert!(app.current_is_favourite());
        assert!(app.message.as_ref().unwrap().starts_with('★'));

        // The same keypress takes the star back off.
        app.toggle_favourite();
        assert!(app.favs.is_empty());
        assert!(!app.current_is_favourite());
    }

    #[test]
    fn the_star_marks_the_exact_pair_solid_and_other_pairings_hollow() {
        let (mut app, _m, _s) = test_app();
        let a = loc("/m/a.mid").unwrap();
        let one = loc("/s/one.sf2").unwrap();
        let two = loc("/s/two.sf2").unwrap();
        app.favs.toggle(a.clone(), one.clone(), 100);

        // MIDI panel: solid only while the pair's own font is the loaded one.
        app.soundfont = Some(one.clone());
        assert_eq!(app.star_for(Panel::Midi, &a), "★");
        app.soundfont = Some(two.clone());
        assert_eq!(app.star_for(Panel::Midi, &a), "☆");
        assert_eq!(app.star_for(Panel::Midi, &loc("/m/b.mid").unwrap()), " ");

        // SoundFont panel: solid only against the track that is playing.
        app.now_playing = Some(a);
        assert_eq!(app.star_for(Panel::Sf2, &one), "★");
        app.now_playing = loc("/m/b.mid");
        assert_eq!(app.star_for(Panel::Sf2, &one), "☆");
        assert_eq!(app.star_for(Panel::Sf2, &two), " ");
    }

    #[test]
    fn favourites_overlay_lists_reorders_and_unstars() {
        let (mut app, _m, _s, _f) = playlist_app(3);
        app.favourites_open();
        let ov = app.overlay.as_ref().expect("overlay open");
        assert_eq!(ov.source, Source::Favourites);
        assert_eq!(ov.rows.len(), 3);
        // Playlist order, cursor on the first entry.
        assert_eq!(ov.selected().unwrap().midi, Some(key_at(&app, 0).0));

        // Tab regroups the history, but the favourites stay one flat list.
        app.overlay_cycle_view();
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 3);
        assert_eq!(app.overlay.as_ref().unwrap().view, View::Pairs);

        // Shift+Down moves the row, and the cursor follows it.
        app.overlay_move(true);
        assert_eq!(app.favs.get(1).unwrap().midi.file_name(), "a.mid");
        let ov = app.overlay.as_ref().unwrap();
        assert_eq!(ov.state.selected(), Some(1));
        assert_eq!(
            ov.selected().unwrap().midi.as_ref().unwrap().file_name(),
            "a.mid"
        );

        // Back up, then past the top: moving off the end is a no-op.
        app.overlay_move(false);
        app.overlay_move(false);
        assert_eq!(app.favs.get(0).unwrap().midi.file_name(), "a.mid");
        assert_eq!(app.overlay.as_ref().unwrap().state.selected(), Some(0));

        // `d` unstars the selected row, and `f` is the same gesture here.
        app.overlay_delete();
        assert_eq!(app.favs.len(), 2);
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 2);
        app.overlay_toggle_favourite();
        assert_eq!(app.favs.len(), 1);

        // There is no erase-all for favourites, so `D` does nothing.
        app.history_clear();
        assert_eq!(app.favs.len(), 1);
        assert!(app.message.is_none());
    }

    #[test]
    fn f_in_the_history_overlay_stars_a_row_but_not_half_a_pair() {
        let (mut app, _m, _s) = test_app();
        app.history
            .record(loc("/m/a.mid"), loc("/s/one.sf2"), 100, true);
        // A SoundFont loaded with nothing playing.
        app.history.record(None, loc("/s/two.sf2"), 200, true);
        app.history_open();

        // The newest row is the font on its own: not a pair, so not starrable.
        app.overlay_toggle_favourite();
        assert!(app.favs.is_empty());
        assert!(app.message.as_ref().unwrap().contains("Not a pair"));

        // The pair below it stars, and un-stars on a second press.
        app.overlay.as_mut().unwrap().move_down(1);
        app.overlay_toggle_favourite();
        assert!(app
            .favs
            .is_pair(&loc("/m/a.mid").unwrap(), &loc("/s/one.sf2").unwrap()));
        app.overlay_toggle_favourite();
        assert!(app.favs.is_empty());
        // Starring a history row leaves the history itself alone.
        assert_eq!(app.history.len(), 2);
    }

    #[test]
    fn the_playlist_advances_in_order_and_stops_at_the_end() {
        let (mut app, _m, _s, font) = playlist_app(3);

        let step = app.next_favourite(&key_at(&app, 0), 0).expect("b.mid");
        assert_eq!(step.midi.file_name(), "b.mid");
        assert_eq!(step.soundfont, Some(font), "each entry brings its own font");
        assert_eq!(step.fav_idx, Some(1));

        // Past the last entry the playlist stops rather than wrapping.
        assert!(app.next_favourite(&key_at(&app, 2), 2).is_none());

        // With repeat on it wraps to the first.
        app.repeat = true;
        let step = app.next_favourite(&key_at(&app, 2), 2).expect("wraps");
        assert_eq!(step.midi.file_name(), "a.mid");
        assert_eq!(step.fav_idx, Some(0));
    }

    #[test]
    fn the_playlist_skips_entries_whose_files_have_gone() {
        let (app, midi_dir, _s, _f) = playlist_app(3);
        // b.mid disappears from under the playlist.
        std::fs::remove_file(midi_dir.path().join("b.mid")).unwrap();
        let step = app.next_favourite(&key_at(&app, 0), 0).expect("c.mid");
        assert_eq!(step.midi.file_name(), "c.mid");
        assert_eq!(step.fav_idx, Some(2));
    }

    #[test]
    fn a_playlist_with_nothing_playable_terminates() {
        let (mut app, midi_dir, _s, _f) = playlist_app(2);
        // Repeat on, so a naive scan would wrap around for ever.
        app.repeat = true;
        for name in ["a.mid", "b.mid"] {
            std::fs::remove_file(midi_dir.path().join(name)).unwrap();
        }
        assert!(app.next_favourite(&key_at(&app, 0), 0).is_none());
    }

    #[test]
    fn unstarring_the_playing_entry_continues_with_the_one_that_took_its_slot() {
        let (mut app, _m, _s, _f) = playlist_app(3);
        let key = key_at(&app, 0);
        // The entry that is playing is un-starred; b.mid now holds slot 0.
        app.favs.remove(0);
        let step = app
            .next_favourite(&key, 0)
            .expect("the entry that moved up");
        assert_eq!(step.midi.file_name(), "b.mid");
        assert_eq!(step.fav_idx, Some(0));
    }

    #[test]
    fn a_reorder_while_playing_is_followed() {
        let (mut app, _m, _s, _f) = playlist_app(3);
        let key = key_at(&app, 0);
        // a.mid is moved to the end while it plays, so what follows it is what
        // follows its new position — not what followed the old one.
        app.favs.swap(0, 2); // c, b, a
        app.repeat = true;
        let step = app.next_favourite(&key, 0).expect("wraps from the end");
        assert_eq!(step.midi.file_name(), "c.mid");
        assert_eq!(step.fav_idx, Some(0));
    }

    #[test]
    fn the_directory_queue_is_unchanged_by_the_favourites() {
        let (mut app, midi_dir, _s, _f) = playlist_app(3);
        // Nothing started the playlist, so the queue is still the directory:
        // the next file on disk, with no SoundFont of its own.
        let a = Location::Fs(midi_dir.path().join("a.mid"));
        let step = app.next_in_source(&a).expect("b.mid follows a.mid");
        assert_eq!(step.midi.file_name(), "b.mid");
        assert_eq!(step.soundfont, None, "the loaded font is kept");
        assert_eq!(step.fav_idx, None);
        assert!(app.playlist_position().is_none());
    }

    #[test]
    fn playing_a_favourite_makes_it_the_queue_and_enter_hands_it_back() {
        let (mut app, midi_dir, _s, _f) = playlist_app(2);
        app.overlay_open(Source::Favourites);
        app.overlay_activate();
        assert!(app.overlay.is_none(), "the overlay closes on activation");
        assert_eq!(app.playlist_position(), Some((1, 2)));

        // Advancing the queue moves the position along with it.
        let step = app.next_favourite(&key_at(&app, 0), 0).expect("b.mid");
        app.play_step(step);
        assert_eq!(app.playlist_position(), Some((2, 2)));
        // The queue's own font load is not a choice the user made, so it adds
        // no history row. (The synth cannot load a stub file under test, so
        // this only guards the load path, not the play.)
        assert!(app.history.is_empty());

        // Playing from the panel hands the queue back to the directory.
        app.midi
            .select_loc(&Location::Fs(midi_dir.path().join("a.mid")));
        app.active = Panel::Midi;
        app.activate_selection();
        assert!(app.playlist_position().is_none());
    }

    #[test]
    fn emptying_the_list_while_it_is_the_queue_hides_the_badge() {
        let (mut app, _m, _s, _f) = playlist_app(2);
        app.play_favourite_at(1);
        assert_eq!(app.playlist_position(), Some((2, 2)));

        // Un-starring everything mid-playlist must not report a position in a
        // list that no longer has one.
        app.favs.remove(1);
        assert_eq!(app.playlist_position(), Some((1, 1)), "clamped to the list");
        app.favs.remove(0);
        assert!(app.playlist_position().is_none());
    }

    #[test]
    fn a_favourite_whose_file_vanished_reports_instead_of_playing() {
        let (mut app, midi_dir, _s, _f) = playlist_app(1);
        std::fs::remove_file(midi_dir.path().join("a.mid")).unwrap();
        app.overlay_open(Source::Favourites);
        app.overlay_activate();

        assert!(app.message.as_ref().unwrap().contains("gone"));
        assert!(app.now_playing.is_none());
        // The failed start did not make the favourites the queue.
        assert!(app.playlist_position().is_none());
        // The favourite is kept: the drive may simply be unmounted.
        assert_eq!(app.favs.len(), 1);
    }
}
