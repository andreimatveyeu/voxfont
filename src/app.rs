//! Application state and the controller logic that ties the two browser panels
//! to the fluidsynth player.

use crate::browser::Browser;
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

/// State of the history overlay while it is open.
pub struct HistoryUi {
    pub view: View,
    pub state: ListState,
    /// The rows of `view` matching `filter`, as currently displayed.
    pub rows: Vec<Row>,
    pub filter: String,
    /// True while `/` is capturing keystrokes into `filter`.
    pub filtering: bool,
    /// Set by the first `D`; the second one actually clears the history.
    confirm_clear: bool,
}

impl HistoryUi {
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
    /// The combination being listened to right now, not yet committed.
    pending: Option<Pending>,
    /// Open history overlay, if any.
    pub hist: Option<HistoryUi>,

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
            pending: None,
            hist: None,
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
            Panel::Sf2 => self.load_soundfont(loc),
            Panel::Midi => self.play_path(loc),
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
    /// | on   | off    | play next; stop after the last file    |
    /// | on   | on     | play next; wrap to first (loop dir)    |
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
            if let Some(next) = self.midi.neighbour_file(&cur, true) {
                self.play_path(next);
            } else if self.repeat {
                // End of directory: loop back to the first file of the playing
                // file's directory (not wherever the user is now browsing).
                match self.midi.first_file_of(&cur) {
                    Some(first) => self.play_path(first),
                    None => self.stop(),
                }
            } else {
                self.stop();
            }
        } else if self.repeat {
            self.play_track(cur, false); // loop the current track
        } else {
            self.stop();
        }
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

    /// Commit whatever is still pending and flush the history — called on exit.
    pub fn finish_history(&mut self) {
        self.commit_history();
        self.history.save();
    }

    pub fn history_open(&mut self) {
        // The overlay and the panel filter are exclusive modes: leaving search
        // restores the full listing behind the overlay.
        if self.search.is_some() {
            self.search_cancel();
        }
        let view = View::Pairs;
        let rows = self.history.rows(view, "");
        let mut state = ListState::default();
        state.select((!rows.is_empty()).then_some(0));
        self.message = self
            .history
            .is_empty()
            .then(|| "History is empty".to_string());
        self.hist = Some(HistoryUi {
            view,
            state,
            rows,
            filter: String::new(),
            filtering: false,
            confirm_clear: false,
        });
    }

    pub fn history_close(&mut self) {
        self.hist = None;
    }

    /// True while the overlay's `/` filter is capturing keystrokes.
    pub fn history_filtering(&self) -> bool {
        self.hist.as_ref().map(|h| h.filtering).unwrap_or(false)
    }

    /// Rebuild the visible rows after a view, filter or content change. The
    /// cursor is kept (clamped) when the rows still describe the same list.
    fn history_rebuild(&mut self, keep_cursor: bool) {
        let (view, filter, cur) = match &self.hist {
            Some(h) => (h.view, h.filter.clone(), h.state.selected().unwrap_or(0)),
            None => return,
        };
        let rows = self.history.rows(view, &filter);
        if let Some(h) = self.hist.as_mut() {
            let idx = match (rows.is_empty(), keep_cursor) {
                (true, _) => None,
                (false, true) => Some(cur.min(rows.len() - 1)),
                (false, false) => Some(0),
            };
            h.rows = rows;
            h.state.select(idx);
        }
    }

    /// Cycle the overlay between the combination, per-track and per-SoundFont
    /// views of the same log.
    pub fn history_cycle_view(&mut self) {
        if let Some(h) = self.hist.as_mut() {
            h.view = h.view.next();
        }
        self.history_rebuild(false);
    }

    pub fn history_start_filter(&mut self) {
        if let Some(h) = self.hist.as_mut() {
            h.filtering = true;
            h.filter.clear();
        }
        self.history_rebuild(false);
    }

    pub fn history_filter_push(&mut self, c: char) {
        if let Some(h) = self.hist.as_mut() {
            h.filter.push(c);
        }
        self.history_rebuild(false);
    }

    pub fn history_filter_backspace(&mut self) {
        if let Some(h) = self.hist.as_mut() {
            h.filter.pop();
        }
        self.history_rebuild(false);
    }

    pub fn history_filter_cancel(&mut self) {
        if let Some(h) = self.hist.as_mut() {
            h.filtering = false;
            h.filter.clear();
        }
        self.history_rebuild(false);
    }

    /// Play the selected entry again exactly as it was heard: its SoundFont is
    /// loaded first (unless it is already the loaded one), then its track. Both
    /// are stored as locations, so an entry that lives inside a zip archive is
    /// extracted and played just like it was the first time.
    pub fn history_restore(&mut self) {
        let row = match self.hist.as_ref().and_then(|h| h.selected()) {
            Some(r) => r.clone(),
            None => return,
        };
        self.hist = None;

        // Check both sides first, so an entry that can no longer be played
        // leaves the panels exactly as they were.
        if let Some(sf) = &row.soundfont {
            if !sf.exists() {
                self.message = Some(format!("SoundFont is gone: {}", sf.display()));
                return;
            }
        }
        if let Some(midi) = &row.midi {
            if !midi.exists() {
                self.message = Some(format!("File is gone: {}", midi.display()));
                return;
            }
        }

        // Follow the entry in the panels before playing it, so the cursor ends
        // up on the file itself — inside the archive when that is where it
        // lives, not parked on the `.zip`.
        if let Some(midi) = &row.midi {
            self.midi.reveal(midi);
        }
        if let Some(sf) = &row.soundfont {
            self.sf2.reveal(sf);
        }

        if let Some(sf) = row.soundfont {
            if self.soundfont.as_ref() != Some(&sf) {
                self.load_soundfont(sf);
            }
        }
        if let Some(midi) = row.midi {
            self.play_path(midi);
        }
    }

    /// Point both panels at the selected entry without playing it, the way `G`
    /// does for what is currently playing.
    pub fn history_reveal(&mut self) {
        let row = match self.hist.as_ref().and_then(|h| h.selected()) {
            Some(r) => r.clone(),
            None => return,
        };
        self.hist = None;
        if let Some(m) = &row.midi {
            self.midi.reveal(m);
        }
        if let Some(sf) = &row.soundfont {
            self.sf2.reveal(sf);
        }
    }

    /// Forget the selected row: in a grouped view that is every entry behind it.
    pub fn history_forget(&mut self) {
        let idxs = match self.hist.as_ref().and_then(|h| h.selected()) {
            Some(r) => r.idxs.clone(),
            None => return,
        };
        self.history.forget(&idxs);
        self.history.save();
        self.history_rebuild(true);
    }

    /// Clear the whole history. The first press only asks; the second one does
    /// it, since there is no undo.
    pub fn history_clear(&mut self) {
        if !self.hist.as_ref().map(|h| h.confirm_clear).unwrap_or(false) {
            if let Some(h) = self.hist.as_mut() {
                h.confirm_clear = true;
            }
            self.message = Some("Press D again to erase the whole history".into());
            return;
        }
        self.history.clear();
        self.history.save();
        if let Some(h) = self.hist.as_mut() {
            h.confirm_clear = false;
        }
        self.history_rebuild(false);
        self.message = Some("History erased".into());
    }

    /// Any other keypress takes back the pending "erase everything" question.
    pub fn history_cancel_confirm(&mut self) {
        if let Some(h) = self.hist.as_mut() {
            if h.confirm_clear {
                h.confirm_clear = false;
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
        let hist = app.hist.as_ref().expect("overlay open");
        assert_eq!(hist.rows.len(), 2);
        assert_eq!(hist.view, View::Pairs);
        // Newest first, cursor on the top row.
        assert_eq!(hist.selected().unwrap().midi, loc("/m/b.mid"));

        // Tab walks the three views of the same log.
        app.history_cycle_view();
        assert_eq!(app.hist.as_ref().unwrap().view, View::Tracks);
        assert_eq!(app.hist.as_ref().unwrap().rows.len(), 2);
        app.history_cycle_view();
        assert_eq!(app.hist.as_ref().unwrap().view, View::Fonts);
        assert_eq!(
            app.hist.as_ref().unwrap().rows.len(),
            1,
            "both plays used the same SoundFont"
        );
        app.history_cycle_view();

        // `/` narrows the list; Esc restores it.
        app.history_start_filter();
        app.history_filter_push('a');
        assert_eq!(app.hist.as_ref().unwrap().rows.len(), 1);
        app.history_filter_cancel();
        assert_eq!(app.hist.as_ref().unwrap().rows.len(), 2);

        // `d` forgets the selected row and keeps the overlay usable.
        app.history_forget();
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.hist.as_ref().unwrap().rows.len(), 1);

        // `D` asks once, then erases.
        app.history_clear();
        assert_eq!(app.history.len(), 1, "the first D only asks");
        app.history_clear();
        assert!(app.history.is_empty());

        app.history_close();
        assert!(app.hist.is_none());
    }

    #[test]
    fn restoring_a_vanished_file_reports_instead_of_playing() {
        let (mut app, _m, _s) = test_app();
        app.history
            .record(loc("/no/such/track.mid"), None, 100, true);
        app.history_open();
        app.history_restore();

        assert!(app.hist.is_none(), "the overlay closes on activation");
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
        app.history_restore();

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
        app.history_restore();
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
        app.history_reveal();

        assert!(app.hist.is_none());
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
}
