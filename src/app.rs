//! Application state and the controller logic that ties the two browser panels
//! to the fluidsynth player.

use crate::browser::Browser;
use crate::favourites::Favourites;
use crate::fluid::Synth;
use crate::history::{self, History, Row, View};
use crate::midi::{self, MidiInfo};
use crate::playlist::{self, Playlist};
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

/// Which store the overlay is showing. All are lists of (track, SoundFont)
/// rows over the same layout and keys, so they share one overlay rather than
/// each growing their own copy of the navigation, filter and reveal logic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    History,
    Favourites,
    Playlist,
}

/// Where the *next* track comes from when the current one finishes. Set by
/// whatever started playing: browsing plays on through the directory, a
/// history row through the history, a favourite through the favourites list, a
/// playlist item through the playlist. There is deliberately no separate
/// "playlist mode" to toggle — as with directory playback, where you started
/// decides what follows.
///
/// The overlays are views beside the panels, and a list only plays on while
/// its view is showing: closing the overlay lets the current track finish and
/// stops there, and reopening it picks the queue up again. See
/// [`App::queue_live`].
#[derive(Clone)]
enum PlaySource {
    Directory,
    /// Playing the history. Playing reorders the log it is built from, so the
    /// queue is the rows as they were shown when `Enter` was pressed, with the
    /// view and filter that produced them; `idx` is the playing row.
    History {
        rows: Vec<Row>,
        view: View,
        filter: String,
        idx: usize,
    },
    /// Playing the favourites list. `key` identifies the entry that is playing
    /// so the position survives a reorder; `idx` is where it sat, used as a
    /// fallback if that entry is un-starred while it plays.
    Favourites {
        key: (Location, Location),
        idx: usize,
    },
    /// Playing the open playlist. A playlist may hold the same pair twice, so
    /// the playing item is followed by its session identity rather than by
    /// its contents; `idx` is the fallback if it is removed while it plays.
    Playlist {
        id: u64,
        idx: usize,
    },
}

/// Which list a [`Step`] came from, and where in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Directory,
    History(usize),
    Favourite(usize),
    Playlist { id: u64, idx: usize },
}

/// The next thing to play, as resolved from the current [`PlaySource`].
struct Step {
    midi: Location,
    /// The SoundFont it must be heard through; `None` keeps the loaded one.
    soundfont: Option<Location>,
    origin: Origin,
}

/// What the bottom-line prompt is asking for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
    /// A directory for the active panel (the `i` key).
    Goto,
    /// A file to save the playlist to (`W`, or `w` on a new playlist).
    SavePlaylist,
}

/// The one-line path prompt, with filename completion.
pub struct Prompt {
    pub kind: PromptKind,
    pub buf: String,
    /// Set when saving would overwrite a different, existing file; a second
    /// Enter on the same path goes ahead. Any edit takes it back.
    confirm: Option<PathBuf>,
}

/// An action held back because it would drop unsaved playlist edits, waiting
/// for the same key again to confirm it.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Discard {
    Quit,
    Open(PathBuf),
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
    /// The starred (track, SoundFont) pairs, which double as a playlist.
    /// Starts in-memory only; `main` swaps in the persisted one. `--no-history`
    /// does not affect it: a star is a deliberate act, not a recording.
    pub favs: Favourites,
    /// The open playlist file — or a new, unsaved one. Saved only on request.
    pub playlist: Playlist,
    /// The SoundFont a playlist item without one of its own plays through: the
    /// font last chosen, by hand or by replaying a history entry or favourite,
    /// or restored from the session. Fonts that playlist and favourites items
    /// load for themselves do not change it, so after an item pinned to one
    /// font, the next unpinned item goes back to the user's choice.
    pub user_font: Option<Location>,
    /// A quit or playlist switch waiting for confirmation because the playlist
    /// has unsaved changes.
    discard_armed: Option<Discard>,
    /// Whether [`Self::save_state`] writes the session file. Off by default,
    /// like the in-memory history and favourites, so tests never overwrite the
    /// real `state.conf`; `main` turns it on.
    pub persist_session: bool,
    /// The combination being listened to right now, not yet committed.
    pending: Option<Pending>,
    /// Open history / favourites overlay, if any.
    pub overlay: Option<Overlay>,
    /// Where the next track comes from when this one ends.
    play_source: PlaySource,
    /// Set when playback stopped at the end of a track only because the
    /// queue's overlay was closed, so reopening it can put the cursor on what
    /// would have played next. Cleared whenever a track starts.
    queue_held: bool,

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
    /// The open path prompt (go to directory, save playlist), if any.
    pub prompt: Option<Prompt>,
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
            midi: Browser::new_at(midi_dir, MIDI_EXTS, true, true)
                .also_listing(playlist::PLAYLIST_EXTS),
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
            playlist: Playlist::default(),
            user_font: None,
            discard_armed: None,
            persist_session: false,
            pending: None,
            overlay: None,
            play_source: PlaySource::Directory,
            queue_held: false,
            play_temp: None,
            sf_temp: None,
            volume,
            repeat: false,
            next_mode: true,
            message: warn,
            search: None,
            prompt: None,
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
        if self.active == Panel::Midi && playlist::is_playlist_name(&loc.file_name()) {
            if let Some(p) = loc.as_fs() {
                self.open_playlist(p.to_path_buf());
            }
            return;
        }
        self.discard_armed = None;
        match self.active {
            // Loading a font by hand is the A/B move; it does not leave the
            // favourites or the playlist, whose next entry brings its own font
            // (or, in a playlist, plays through this one).
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

    /// Load a SoundFont the user chose. It becomes the user's font.
    pub fn load_soundfont(&mut self, loc: Location) {
        if self.load_font(loc.clone(), true) {
            self.user_font = Some(loc);
        }
    }

    /// Load the SoundFont remembered from the last session without writing a
    /// history entry: the user did not pick it this time, and an automatic
    /// restore must not push it to the top of the history on every launch. It
    /// is still the user's font.
    pub fn restore_soundfont(&mut self, loc: Location) {
        if self.load_font(loc.clone(), false) {
            self.user_font = Some(loc);
        }
    }

    /// Load `loc` into the synth, remembering the combination when `remember`.
    /// Returns true when the font loaded.
    fn load_font(&mut self, loc: Location, remember: bool) -> bool {
        // Archive members are extracted to a temp file first, since the FFI
        // loads SoundFonts by filename only.
        let (path, guard) = match vfs::resolve_to_file(&loc) {
            Ok(r) => r,
            Err(e) => {
                self.message = Some(e);
                return false;
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
                true
            }
            Err(e) => {
                self.message = Some(e);
                false
            }
        }
    }

    /// Persist the current directories, the last-played MIDI file and the loaded
    /// SoundFont for next launch. Directories and the played file keep their full
    /// location, so an archive (or a file inside one) is restored as such.
    pub fn save_state(&self) {
        if !self.persist_session {
            return;
        }
        crate::state::save(&crate::state::State {
            midi_dir: Some(self.midi.location()),
            midi_file: self.last_played.clone(),
            sf2_dir: Some(self.sf2.location()),
            soundfont: self.soundfont.clone(),
            playlist: self.playlist.path().cloned(),
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
                self.queue_held = false;
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
    /// entry of the favourites or the playlist when one of those started this
    /// track — see [`PlaySource`]. The table itself is the same either way.
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

        self.track_ended(cur);
    }

    /// Apply the next/repeat modes to `cur`, which has just finished.
    fn track_ended(&mut self, cur: Location) {
        if self.next_mode {
            match self.next_in_source(&cur) {
                Some(step) => self.play_step(step),
                None => {
                    // A queue held back only by its closed overlay still has
                    // somewhere to go: reopening it puts the cursor there.
                    let held = !self.queue_live() && self.next_in_list().is_some();
                    self.stop();
                    self.queue_held = held;
                }
            }
        } else if self.repeat {
            self.play_track(cur, false); // loop the current track
        } else {
            self.stop();
        }
    }

    /// What follows `cur`, according to the current play source. `None` means
    /// there is nothing left to play and the player should stop — which is
    /// also the case while the queue's overlay is closed.
    fn next_in_source(&mut self, cur: &Location) -> Option<Step> {
        if !self.queue_live() {
            return None;
        }
        match &self.play_source {
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
                    origin: Origin::Directory,
                })
            }
            _ => self.next_in_list(),
        }
    }

    /// What follows the playing entry when the queue is a list (the history,
    /// the favourites or the playlist), whether or not its overlay is open.
    fn next_in_list(&self) -> Option<Step> {
        match &self.play_source {
            PlaySource::Directory => None,
            PlaySource::History { rows, idx, .. } => self.next_history_row(rows, *idx),
            PlaySource::Favourites { key, idx } => self.next_favourite(key, *idx),
            PlaySource::Playlist { id, idx } => self.next_playlist_item(*id, *idx),
        }
    }

    /// Which overlay the queue belongs to; `None` for the directory.
    fn queue_source(&self) -> Option<Source> {
        match self.play_source {
            PlaySource::Directory => None,
            PlaySource::History { .. } => Some(Source::History),
            PlaySource::Favourites { .. } => Some(Source::Favourites),
            PlaySource::Playlist { .. } => Some(Source::Playlist),
        }
    }

    /// Whether the queue plays on past the current track. A list does only
    /// while its overlay is open; the queue itself is kept while the overlay
    /// is closed, so reopening it carries on from where it was. The directory
    /// always plays on.
    pub fn queue_live(&self) -> bool {
        match self.queue_source() {
            None => true,
            source => self.overlay_source() == source,
        }
    }

    /// Walk forward through the history rows the queue was started from,
    /// skipping rows without a track (a SoundFont on its own) and rows whose
    /// files have gone, with the same one-pass cap as [`Self::next_favourite`].
    /// A row without a SoundFont plays through the loaded one.
    fn next_history_row(&self, rows: &[Row], idx: usize) -> Option<Step> {
        let n = rows.len();
        for offset in 1..=n {
            let i = match idx + offset {
                i if i < n => i,
                i if self.repeat => i % n,
                _ => return None,
            };
            let row = &rows[i];
            let midi = match &row.midi {
                Some(m) if m.exists() => m.clone(),
                _ => continue,
            };
            if row.soundfont.as_ref().is_some_and(|sf| !sf.exists()) {
                continue;
            }
            return Some(Step {
                midi,
                soundfont: row.soundfont.clone(),
                origin: Origin::History(i),
            });
        }
        None
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
                    origin: Origin::Favourite(i),
                });
            }
        }
        None
    }

    /// Walk forward through the playlist from the item that just played,
    /// skipping any that cannot be played now, with the same one-pass cap and
    /// reorder-following as [`Self::next_favourite`].
    fn next_playlist_item(&self, id: u64, idx: usize) -> Option<Step> {
        let n = self.playlist.len();
        if n == 0 {
            return None;
        }
        let (at, first) = match self.playlist.position_of(id) {
            Some(p) => (p, 1),
            None => (idx, 0),
        };
        for offset in first..first + n {
            let i = match at + offset {
                i if i < n => i,
                i if self.repeat => i % n,
                _ => return None,
            };
            if let Some(step) = self.playlist_step(i) {
                return Some(step);
            }
        }
        None
    }

    /// The step for playlist item `i`, if it can be played now: its track is
    /// there, and so is the font it plays through — its own, or the user's.
    fn playlist_step(&self, i: usize) -> Option<Step> {
        let item = self.playlist.get(i)?;
        let font = item.soundfont.clone().or_else(|| self.user_font.clone())?;
        if !(item.midi.exists() && font.exists()) {
            return None;
        }
        Some(Step {
            midi: item.midi.clone(),
            soundfont: Some(font),
            origin: Origin::Playlist {
                id: self.playlist.id_at(i)?,
                idx: i,
            },
        })
    }

    /// Play what [`Self::next_in_source`] resolved, loading the step's own
    /// SoundFont first when it brought one.
    fn play_step(&mut self, step: Step) {
        // Advancing the queue is not a choice the user made, so the font is
        // loaded without a history record: the play itself is recorded, and a
        // font-only row (or one pairing the finished track with the new font)
        // would be noise. Nor does it become the user's font.
        if let Some(sf) = &step.soundfont {
            if self.soundfont.as_ref() != Some(sf) {
                self.load_font(sf.clone(), false);
            }
        }
        // The history and the favourites point back at files that live
        // somewhere, so the panels follow each entry there, as they do when
        // one is picked with Enter. A playlist does not move them.
        if matches!(step.origin, Origin::History(_) | Origin::Favourite(_)) {
            self.midi.reveal(&step.midi);
            if let Some(sf) = &step.soundfont {
                self.sf2.reveal(sf);
            }
        }
        match (step.origin, step.soundfont) {
            (Origin::History(i), _) => {
                if let PlaySource::History { idx, .. } = &mut self.play_source {
                    *idx = i;
                }
            }
            (Origin::Favourite(idx), Some(sf)) => {
                self.play_source = PlaySource::Favourites {
                    key: (step.midi.clone(), sf),
                    idx,
                };
            }
            (Origin::Playlist { id, idx }, _) => {
                self.play_source = PlaySource::Playlist { id, idx };
            }
            _ => {}
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
            // An overlay stays open while its list plays, so keep the play
            // counts current. The favourites' and the playlist's rows are the
            // list itself, so the cursor stays put; the history's would shift
            // under it, and while it is the queue they are the queue's rows.
            if matches!(
                self.overlay_source(),
                Some(Source::Favourites | Source::Playlist)
            ) {
                self.overlay_rebuild(true);
            }
        }
    }

    /// Commit whatever is still pending and flush both stores — called on exit.
    /// The favourites save on every change, so this only catches a write that
    /// failed earlier. The playlist is not saved here: saving it is explicit,
    /// and quitting with unsaved edits has already been confirmed.
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

    /// Which list is the queue, with the (1-based position, length) of what is
    /// playing in it. `None` while the queue is the directory.
    pub fn queue_position(&self) -> Option<(Source, usize, usize)> {
        // Either list can be emptied, or shortened, while it is the queue.
        let (source, found, idx, n) = match &self.play_source {
            PlaySource::Directory => return None,
            PlaySource::History { rows, idx, .. } => (Source::History, None, *idx, rows.len()),
            PlaySource::Favourites { key, idx } => (
                Source::Favourites,
                self.favs.position_of(&key.0, &key.1),
                *idx,
                self.favs.len(),
            ),
            PlaySource::Playlist { id, idx } => (
                Source::Playlist,
                self.playlist.position_of(*id),
                *idx,
                self.playlist.len(),
            ),
        };
        if n == 0 {
            return None;
        }
        Some((source, found.unwrap_or(idx).min(n - 1) + 1, n))
    }

    // --- the history / favourites overlay (the `R` and `F` keys) --------------

    pub fn history_open(&mut self) {
        self.overlay_open(Source::History);
    }

    pub fn favourites_open(&mut self) {
        self.overlay_open(Source::Favourites);
    }

    pub fn playlist_open(&mut self) {
        self.overlay_open(Source::Playlist);
    }

    fn overlay_open(&mut self, source: Source) {
        // The overlay and the panel filter are exclusive modes: leaving search
        // restores the full listing behind the overlay.
        if self.search.is_some() {
            self.search_cancel();
        }
        // While the history is the queue, its overlay reopens on the queue's
        // rows rather than on the log as it has since become.
        let (view, filter, rows) = match &self.play_source {
            PlaySource::History {
                rows, view, filter, ..
            } if source == Source::History => (*view, filter.clone(), rows.clone()),
            _ => (
                View::Pairs,
                String::new(),
                self.overlay_rows(source, View::Pairs, ""),
            ),
        };
        let mut state = ListState::default();
        let at = self.queue_cursor(source, &rows).unwrap_or(0);
        state.select((!rows.is_empty()).then(|| at.min(rows.len() - 1)));
        self.message = match source {
            Source::History if self.history.is_empty() => Some("History is empty".to_string()),
            Source::Favourites if self.favs.is_empty() => {
                Some("No favourites yet — press f while a track is playing".to_string())
            }
            Source::Playlist if self.playlist.is_empty() => Some(
                "The playlist is empty — press a on a MIDI file to add it (A: with the loaded SoundFont)"
                    .to_string(),
            ),
            _ => None,
        };
        self.overlay = Some(Overlay {
            source,
            view,
            state,
            rows,
            filter,
            filtering: false,
            confirm_clear: false,
        });
    }

    /// Where the cursor goes when `source`'s overlay opens on `rows` while
    /// that list is the queue: on the track playing, or — when playback
    /// stopped only because the overlay was closed — on the one that would
    /// have followed, so `Enter` carries on from there.
    fn queue_cursor(&self, source: Source, rows: &[Row]) -> Option<usize> {
        if self.queue_source() != Some(source) {
            return None;
        }
        let next = match self.queue_held {
            true => self.next_in_list().map(|s| s.origin),
            false => None,
        };
        let i = match (next, &self.play_source) {
            (Some(Origin::History(i) | Origin::Favourite(i)), _) => i,
            (Some(Origin::Playlist { idx, .. }), _) => idx,
            (_, PlaySource::History { idx, .. }) => *idx,
            _ => self.queue_position()?.1 - 1,
        };
        match source {
            // The history's rows are the queue's own, one for one.
            Source::History => Some(i),
            _ => rows.iter().position(|r| r.idxs.first() == Some(&i)),
        }
    }

    fn overlay_rows(&self, source: Source, view: View, filter: &str) -> Vec<Row> {
        match source {
            Source::History => self.history.rows(view, filter),
            Source::Favourites => self.favs.rows(filter, &self.history),
            Source::Playlist => self
                .playlist
                .rows(filter, &self.history, self.user_font.as_ref()),
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

    /// Play the selected row and make its list the queue, from that row on.
    /// From the history that means hearing the entry again exactly as it was,
    /// then the rows below it as they are shown now. The overlay stays open,
    /// since it is what keeps the list playing; only its filter stops
    /// capturing keys, so the transport keys work.
    pub fn overlay_activate(&mut self) {
        let (source, row, at) = match self.overlay.as_mut() {
            Some(o) => {
                o.filtering = false;
                match (o.selected(), o.state.selected()) {
                    (Some(r), Some(at)) => (o.source, r.clone(), at),
                    _ => return,
                }
            }
            None => return,
        };
        match source {
            Source::History => {
                self.play_source = PlaySource::Directory;
                let track = row.midi.is_some();
                // A SoundFont-only row just loads the font; there is no track
                // to start the queue with.
                if self.play_combination(row.midi, row.soundfont) && track {
                    if let Some(o) = &self.overlay {
                        self.play_source = PlaySource::History {
                            rows: o.rows.clone(),
                            view: o.view,
                            filter: o.filter.clone(),
                            idx: at,
                        };
                    }
                }
            }
            Source::Favourites => {
                if let Some(&idx) = row.idxs.first() {
                    self.play_favourite_at(idx);
                }
            }
            Source::Playlist => {
                if let Some(&idx) = row.idxs.first() {
                    self.play_playlist_at(idx);
                }
            }
        }
    }

    /// Start (or restart) the playlist at item `idx`. Like a favourite, an
    /// item that cannot be played reports why and leaves everything as it was.
    fn play_playlist_at(&mut self, idx: usize) {
        let item = match self.playlist.get(idx) {
            Some(i) => i.clone(),
            None => return,
        };
        let font = match item.soundfont.clone().or_else(|| self.user_font.clone()) {
            Some(f) => f,
            None => {
                self.message = Some(
                    "This item has no SoundFont of its own — load one in the right panel first"
                        .into(),
                );
                return;
            }
        };
        if !font.exists() {
            self.message = Some(format!("SoundFont is gone: {}", font.display()));
            return;
        }
        if !item.midi.exists() {
            self.message = Some(format!("File is gone: {}", item.midi.display()));
            return;
        }
        let id = match self.playlist.id_at(idx) {
            Some(id) => id,
            None => return,
        };
        // Unlike the history and the favourites, the panels stay where they
        // are: a playlist is a list in its own right, and the directory it was
        // opened from is where the user means to be when they close it.
        self.play_step(Step {
            midi: item.midi,
            soundfont: Some(font),
            origin: Origin::Playlist { id, idx },
        });
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
    /// every entry behind it), take the star off a favourite, or remove the
    /// item from the playlist.
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
            Source::Playlist => {
                if let Some(&i) = idxs.first() {
                    self.playlist.remove(i);
                }
            }
        }
        self.overlay_rebuild(true);
    }

    /// `f` inside an overlay: star the selected history row or playlist item
    /// (a playlist item through the font it plays through), or un-star the
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
        let soundfont = match source {
            Source::Playlist => row.soundfont.or_else(|| self.user_font.clone()),
            _ => row.soundfont,
        };
        match (row.midi, soundfont) {
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

    /// Move the selected favourite or playlist item one place along its list,
    /// the cursor following it. Under a filter the row swaps with its visible
    /// neighbour, so the move is always the one on screen.
    pub fn overlay_move(&mut self, down: bool) {
        let (source, i, rows) = match self.overlay.as_ref() {
            Some(o) if o.source != Source::History => {
                (o.source, o.state.selected().unwrap_or(0), o.rows.clone())
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
        if source == Source::Playlist {
            self.playlist.swap(a, b);
        } else {
            self.favs.swap(a, b);
            self.favs.save();
        }
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

    // --- the playlist (the `P`, `a` and `A` keys) ------------------------------

    /// Read the playlist at `path` into the app, replacing the open one. Used
    /// at launch; [`Self::open_playlist`] is the interactive route, which
    /// guards unsaved edits first.
    pub fn load_playlist(&mut self, path: &std::path::Path) -> Result<(), String> {
        let (list, ignored) = Playlist::load(path)?;
        let n = list.len();
        let mut msg = format!(
            "Playlist: {} · {n} item{}",
            list.name(),
            if n == 1 { "" } else { "s" }
        );
        if ignored > 0 {
            msg.push_str(&format!(
                " · {ignored} line{} ignored",
                if ignored == 1 { "" } else { "s" }
            ));
        }
        self.playlist = list;
        // The queue's item identities belonged to the list just replaced.
        if matches!(self.play_source, PlaySource::Playlist { .. }) {
            self.play_source = PlaySource::Directory;
        }
        self.message = Some(msg);
        Ok(())
    }

    /// Open a playlist file from the MIDI panel into the overlay, without
    /// playing it. Unsaved edits to the open playlist are only dropped when
    /// the same file is opened a second time in a row.
    pub fn open_playlist(&mut self, path: PathBuf) {
        let path = playlist::absolute(&path);
        let armed = Some(Discard::Open(path.clone()));
        if self.playlist.is_dirty() && self.discard_armed != armed {
            self.discard_armed = armed;
            self.message = Some(format!(
                "The playlist has unsaved changes — Enter again to discard them and open {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            return;
        }
        self.discard_armed = None;
        match self.load_playlist(&path) {
            Ok(()) => {
                // Keep the message the overlay would replace with its own.
                let msg = self.message.take();
                self.playlist_open();
                self.message = self.message.take().or(msg);
                self.save_state();
            }
            Err(e) => self.message = Some(e),
        }
    }

    /// Quit, unless that would lose playlist edits: then the first `q` only
    /// warns, and a second one in a row quits.
    pub fn request_quit(&mut self) {
        if self.playlist.is_dirty() && self.discard_armed != Some(Discard::Quit) {
            self.discard_armed = Some(Discard::Quit);
            self.message = Some(
                "The playlist has unsaved changes — q again to quit without saving (P, then w saves)"
                    .into(),
            );
            return;
        }
        self.quit = true;
    }

    /// Take back a pending "discard unsaved changes?" question.
    pub fn disarm_discard(&mut self) {
        if self.discard_armed.take().is_some() {
            self.message = None;
        }
    }

    /// Append the MIDI file under the cursor to the playlist, without a
    /// SoundFont of its own or (`with_font`) pinned to the loaded one. The
    /// cursor moves on, so a run of tracks is added by pressing the key again.
    pub fn playlist_add(&mut self, with_font: bool) {
        let entry = match (self.active, self.midi.selected()) {
            (Panel::Midi, Some(e)) if !e.is_dir && !playlist::is_playlist_name(&e.name) => {
                e.clone()
            }
            _ => {
                self.message =
                    Some("Put the cursor on a MIDI file in the left panel to add it".into());
                return;
            }
        };
        let font = match (with_font, self.soundfont.clone()) {
            (false, _) => None,
            (true, Some(sf)) => Some(sf),
            (true, None) => {
                self.message =
                    Some("No SoundFont loaded — A adds the track with the loaded SoundFont".into());
                return;
            }
        };
        self.message = Some(match &font {
            Some(sf) => format!(
                "Added to the playlist ({}): {} + {}",
                self.playlist.len() + 1,
                entry.name,
                sf.file_name()
            ),
            None => format!(
                "Added to the playlist ({}): {}",
                self.playlist.len() + 1,
                entry.name
            ),
        });
        self.playlist.push(entry.loc, font);
        self.midi.move_down(1);
    }

    /// `s` / `x` in the playlist overlay: pin the loaded SoundFont to the
    /// selected item, or (`pin` false) unpin it so it plays through the user's
    /// font.
    pub fn overlay_set_font(&mut self, pin: bool) {
        let idx = match self.overlay.as_ref() {
            Some(o) if o.source == Source::Playlist => match o.selected() {
                Some(r) => r.idxs[0],
                None => return,
            },
            _ => return,
        };
        let font = match (pin, self.soundfont.clone()) {
            (false, _) => None,
            (true, Some(sf)) => Some(sf),
            (true, None) => {
                self.message = Some("No SoundFont loaded to pin to the item".into());
                return;
            }
        };
        self.playlist.set_font(idx, font);
        self.overlay_rebuild(true);
    }

    /// `w` / `W` in the playlist overlay: save to the playlist's own file, or
    /// ask for one (always, for `W`; for `w`, when the list is new).
    pub fn overlay_save(&mut self, save_as: bool) {
        if self.overlay_source() != Some(Source::Playlist) {
            return;
        }
        if save_as || self.playlist.path().is_none() {
            let buf = match self.playlist.path() {
                Some(p) => p.to_string_lossy().into_owned(),
                None => self
                    .midi
                    .fs_dir()
                    .join("playlist.m3u")
                    .to_string_lossy()
                    .into_owned(),
            };
            self.prompt = Some(Prompt {
                kind: PromptKind::SavePlaylist,
                buf,
                confirm: None,
            });
            return;
        }
        self.save_playlist_as(None);
    }

    /// Save the playlist to `path`, or to its own file with `None`.
    fn save_playlist_as(&mut self, path: Option<PathBuf>) -> bool {
        let res = match path {
            Some(p) => self.playlist.save_as(p),
            None => self.playlist.save(),
        };
        match res {
            Ok(()) => {
                let n = self.playlist.len();
                self.message = Some(format!(
                    "Saved {} ({n} item{})",
                    self.playlist
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    if n == 1 { "" } else { "s" }
                ));
                self.save_state();
                true
            }
            Err(e) => {
                self.message = Some(e);
                false
            }
        }
    }

    // --- the path prompt (`i`, and saving a playlist) ---------------------------

    /// Open the GO prompt, pre-filled with the active panel's directory. The
    /// prompt navigates the filesystem only, so an archive resolves to the
    /// directory that holds it.
    pub fn start_goto(&mut self) {
        let mut d = self.active_browser().fs_dir().to_string_lossy().to_string();
        if !d.ends_with('/') {
            d.push('/');
        }
        self.prompt = Some(Prompt {
            kind: PromptKind::Goto,
            buf: d,
            confirm: None,
        });
    }

    /// Change the prompt's text. Any edit takes back an overwrite question.
    fn prompt_edit(&mut self, f: impl FnOnce(&mut String)) {
        if let Some(p) = self.prompt.as_mut() {
            f(&mut p.buf);
            p.confirm = None;
        }
    }

    pub fn prompt_push(&mut self, c: char) {
        self.prompt_edit(|b| b.push(c));
    }

    pub fn prompt_backspace(&mut self) {
        self.prompt_edit(|b| {
            b.pop();
        });
    }

    /// Delete the last path component (back to the previous slash).
    pub fn prompt_delete_component(&mut self) {
        self.prompt_edit(|b| *b = delete_path_component(b));
    }

    pub fn prompt_cancel(&mut self) {
        self.prompt = None;
    }

    /// Tab-complete the path in the prompt using rustyline's filesystem
    /// completer (handles the directory scan, matching and common-prefix logic).
    pub fn prompt_complete(&mut self) {
        let input = match self.prompt.as_ref() {
            Some(p) => p.buf.clone(),
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
                let done = format!("{}{}", &input[..start], candidates[0].replacement());
                self.prompt_edit(|b| *b = done);
            }
            n => {
                if let Some(lcp) = longest_common_prefix(&candidates) {
                    let done = format!("{}{}", &input[..start], lcp);
                    self.prompt_edit(|b| *b = done);
                }
                self.message = Some(format!("{n} matches"));
            }
        }
    }

    /// Act on the prompt: navigate the active panel, or save the playlist.
    pub fn prompt_submit(&mut self) {
        let (kind, input, confirm) = match self.prompt.as_ref() {
            Some(p) => (p.kind, p.buf.trim().to_string(), p.confirm.clone()),
            None => return,
        };
        match kind {
            PromptKind::Goto => {
                self.prompt = None;
                let path = PathBuf::from(expand_tilde(&input));
                if path.is_dir() {
                    self.message = None;
                    self.active_browser().set_dir(path);
                } else {
                    self.message = Some(format!("Not a directory: {input}"));
                }
            }
            PromptKind::SavePlaylist => {
                if input.is_empty() {
                    self.message = Some("Type a file name to save the playlist to".into());
                    return;
                }
                let mut path = PathBuf::from(expand_tilde(&input));
                if !playlist::is_playlist_name(&input) {
                    let mut s = path.into_os_string();
                    s.push(".m3u");
                    path = PathBuf::from(s);
                }
                let path = playlist::absolute(&path);
                if path.is_dir() {
                    self.message = Some(format!("That is a directory: {}", path.display()));
                    return;
                }
                // Overwriting some other file asks first; re-saving the
                // playlist's own file does not.
                let other = path.exists() && self.playlist.path() != Some(&path);
                if other && confirm.as_ref() != Some(&path) {
                    self.message = Some(format!(
                        "{} exists — Enter again to overwrite it",
                        path.display()
                    ));
                    if let Some(p) = self.prompt.as_mut() {
                        p.confirm = Some(path);
                    }
                    return;
                }
                // A failed write leaves the prompt open to fix the path.
                if self.save_playlist_as(Some(path)) {
                    self.prompt = None;
                }
            }
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

pub(crate) fn expand_tilde(s: &str) -> String {
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

        assert!(
            app.overlay.is_some(),
            "the overlay stays open on activation"
        );
        assert!(app.message.as_ref().unwrap().contains("gone"));
        assert!(app.now_playing.is_none());
        assert!(
            app.queue_position().is_none(),
            "a failed start is not the queue"
        );
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

        // A plain file moves the cursor the same way. (Hand the queue back
        // first: while the history is the queue, its overlay reopens on the
        // queue's rows, which predate this entry.)
        app.play_source = PlaySource::Directory;
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
        assert_eq!(step.origin, Origin::Favourite(1));

        // Past the last entry the playlist stops rather than wrapping.
        assert!(app.next_favourite(&key_at(&app, 2), 2).is_none());

        // With repeat on it wraps to the first.
        app.repeat = true;
        let step = app.next_favourite(&key_at(&app, 2), 2).expect("wraps");
        assert_eq!(step.midi.file_name(), "a.mid");
        assert_eq!(step.origin, Origin::Favourite(0));
    }

    #[test]
    fn the_playlist_skips_entries_whose_files_have_gone() {
        let (app, midi_dir, _s, _f) = playlist_app(3);
        // b.mid disappears from under the playlist.
        std::fs::remove_file(midi_dir.path().join("b.mid")).unwrap();
        let step = app.next_favourite(&key_at(&app, 0), 0).expect("c.mid");
        assert_eq!(step.midi.file_name(), "c.mid");
        assert_eq!(step.origin, Origin::Favourite(2));
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
        assert_eq!(step.origin, Origin::Favourite(0));
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
        assert_eq!(step.origin, Origin::Favourite(0));
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
        assert_eq!(step.origin, Origin::Directory);
        assert!(app.queue_position().is_none());
    }

    #[test]
    fn playing_a_favourite_makes_it_the_queue_and_enter_hands_it_back() {
        let (mut app, midi_dir, _s, _f) = playlist_app(2);
        app.overlay_open(Source::Favourites);
        app.overlay_activate();
        assert!(
            app.overlay.is_some(),
            "the overlay stays open on activation"
        );
        assert_eq!(app.queue_position(), Some((Source::Favourites, 1, 2)));

        // Advancing the queue moves the position along with it.
        let step = app.next_favourite(&key_at(&app, 0), 0).expect("b.mid");
        app.play_step(step);
        assert_eq!(app.queue_position(), Some((Source::Favourites, 2, 2)));
        // The queue's own font load is not a choice the user made, so it adds
        // no history row. (The synth cannot load a stub file under test, so
        // this only guards the load path, not the play.)
        assert!(app.history.is_empty());

        // Playing from the panel hands the queue back to the directory.
        app.midi
            .select_loc(&Location::Fs(midi_dir.path().join("a.mid")));
        app.active = Panel::Midi;
        app.activate_selection();
        assert!(app.queue_position().is_none());
    }

    #[test]
    fn emptying_the_list_while_it_is_the_queue_hides_the_badge() {
        let (mut app, _m, _s, _f) = playlist_app(2);
        app.play_favourite_at(1);
        assert_eq!(app.queue_position(), Some((Source::Favourites, 2, 2)));

        // Un-starring everything mid-playlist must not report a position in a
        // list that no longer has one.
        app.favs.remove(1);
        assert_eq!(
            app.queue_position(),
            Some((Source::Favourites, 1, 1)),
            "clamped to the list"
        );
        app.favs.remove(0);
        assert!(app.queue_position().is_none());
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
        assert!(app.queue_position().is_none());
        // The favourite is kept: the drive may simply be unmounted.
        assert_eq!(app.favs.len(), 1);
    }

    /// An app whose playlist holds `a.mid` pinned to `one.sf2`, then `b.mid`
    /// and `c.mid` with no font, all on disk, plus a second font `two.sf2`.
    fn list_app() -> (App, tempfile::TempDir, tempfile::TempDir) {
        let (mut app, midi_dir, sf_dir) = test_app();
        for n in ["a.mid", "b.mid", "c.mid"] {
            std::fs::write(midi_dir.path().join(n), b"x").unwrap();
        }
        for n in ["one.sf2", "two.sf2"] {
            std::fs::write(sf_dir.path().join(n), b"x").unwrap();
        }
        let m = |n: &str| Location::Fs(midi_dir.path().join(n));
        app.playlist.push(
            m("a.mid"),
            Some(Location::Fs(sf_dir.path().join("one.sf2"))),
        );
        app.playlist.push(m("b.mid"), None);
        app.playlist.push(m("c.mid"), None);
        app.midi.refresh();
        app.sf2.refresh();
        (app, midi_dir, sf_dir)
    }

    fn list_id(app: &App, idx: usize) -> u64 {
        app.playlist.id_at(idx).unwrap()
    }

    #[test]
    fn an_item_without_a_font_plays_through_the_users_font() {
        let (mut app, _m, sf_dir) = list_app();
        let two = Location::Fs(sf_dir.path().join("two.sf2"));
        app.user_font = Some(two.clone());

        let step = app.next_playlist_item(list_id(&app, 0), 0).expect("b.mid");
        assert_eq!(step.midi.file_name(), "b.mid");
        // Not one.sf2, which the item before it brought along.
        assert_eq!(step.soundfont, Some(two));
        assert_eq!(
            step.origin,
            Origin::Playlist {
                id: list_id(&app, 1),
                idx: 1
            }
        );

        // With repeat on it wraps round to the item with its own font.
        app.repeat = true;
        let step = app.next_playlist_item(list_id(&app, 2), 2).expect("wraps");
        assert_eq!(
            step.soundfont,
            Some(Location::Fs(sf_dir.path().join("one.sf2")))
        );
        app.repeat = false;
        assert!(app.next_playlist_item(list_id(&app, 2), 2).is_none());
    }

    #[test]
    fn items_without_a_font_are_skipped_until_the_user_has_one() {
        let (mut app, _m, _s) = list_app();
        assert!(app.user_font.is_none());
        app.repeat = true;
        // b and c have nothing to play through; only a can play.
        let step = app.next_playlist_item(list_id(&app, 0), 0).expect("a.mid");
        assert_eq!(step.midi.file_name(), "a.mid");

        app.play_playlist_at(1);
        assert!(app.message.as_ref().unwrap().contains("no SoundFont"));
        assert!(
            app.queue_position().is_none(),
            "a failed start is not the queue"
        );
    }

    #[test]
    fn fonts_the_queue_loads_do_not_become_the_users_font() {
        let (mut app, _m, sf_dir) = list_app();
        let two = Location::Fs(sf_dir.path().join("two.sf2"));
        app.user_font = Some(two.clone());
        app.play_playlist_at(0);
        assert_eq!(app.queue_position(), Some((Source::Playlist, 1, 3)));
        assert_eq!(app.user_font, Some(two));
    }

    #[test]
    fn the_playlist_queue_follows_its_item_through_edits() {
        let (mut app, _m, sf_dir) = list_app();
        app.user_font = Some(Location::Fs(sf_dir.path().join("two.sf2")));
        app.play_playlist_at(1);
        assert_eq!(app.queue_position(), Some((Source::Playlist, 2, 3)));

        app.playlist.swap(0, 1);
        assert_eq!(app.queue_position(), Some((Source::Playlist, 1, 3)));
        // Removed while playing: carry on with what took its slot.
        let id = list_id(&app, 0);
        app.playlist.remove(0);
        let step = app
            .next_playlist_item(id, 0)
            .expect("the item that moved up");
        assert_eq!(step.midi.file_name(), "a.mid");
    }

    #[test]
    fn the_playlist_plays_on_only_while_its_overlay_is_open() {
        let (mut app, _m, sf_dir) = list_app();
        app.user_font = Some(Location::Fs(sf_dir.path().join("two.sf2")));
        app.playlist_open();
        app.overlay_start_filter();
        app.overlay_activate();
        // Playing an item leaves the overlay open, no longer filtering.
        assert_eq!(app.overlay_source(), Some(Source::Playlist));
        assert!(!app.overlay_filtering());
        assert_eq!(app.queue_position(), Some((Source::Playlist, 1, 3)));
        let a = app.playlist.get(0).unwrap().midi.clone();
        let step = app.next_in_source(&a).expect("b.mid follows while open");
        assert_eq!(step.midi.file_name(), "b.mid");

        // Closed: the queue is kept, but nothing follows the current item.
        app.overlay_close();
        assert!(app.next_in_source(&a).is_none());
        assert_eq!(app.queue_position(), Some((Source::Playlist, 1, 3)));
        // Nor while another overlay has taken its place.
        app.history_open();
        assert!(app.next_in_source(&a).is_none());

        // Reopened on the playing item, and the list carries on.
        app.play_step(step);
        app.overlay_close();
        app.playlist_open();
        assert_eq!(app.overlay.as_ref().unwrap().state.selected(), Some(1));
        let b = app.playlist.get(1).unwrap().midi.clone();
        let step = app.next_in_source(&b).expect("c.mid follows once reopened");
        assert_eq!(step.midi.file_name(), "c.mid");
    }

    #[test]
    fn a_queue_stopped_by_its_closed_overlay_reopens_on_what_follows() {
        let (mut app, _m, sf_dir) = list_app();
        app.user_font = Some(Location::Fs(sf_dir.path().join("two.sf2")));
        app.playlist_open();
        app.overlay_activate();
        app.overlay_close();
        // The item ends with the overlay closed: playback stops...
        app.next_mode = true;
        app.state = PlayState::Playing;
        let a = app.playlist.get(0).unwrap().midi.clone();
        app.track_ended(a);
        assert!(app.queue_held);
        // ...and reopening does not resume it, but lands on the next item.
        app.playlist_open();
        assert!(app.state == PlayState::Stopped, "not resumed");
        assert_eq!(app.overlay.as_ref().unwrap().state.selected(), Some(1));

        // A stop by hand leaves the cursor on the item that was playing.
        app.queue_held = false;
        app.playlist_open();
        assert_eq!(app.overlay.as_ref().unwrap().state.selected(), Some(0));
    }

    #[test]
    fn the_history_is_a_queue_of_its_rows_as_shown() {
        let (mut app, midi_dir, sf_dir) = test_app();
        let font = Location::Fs(sf_dir.path().join("f.sf2"));
        std::fs::write(sf_dir.path().join("f.sf2"), b"x").unwrap();
        for (n, when) in [("a.mid", 300), ("b.mid", 200), ("c.mid", 100)] {
            std::fs::write(midi_dir.path().join(n), b"x").unwrap();
            let m = Location::Fs(midi_dir.path().join(n));
            app.history.record(Some(m), Some(font.clone()), when, true);
        }
        // A SoundFont-only row is skipped by the queue.
        app.history.record(None, Some(font.clone()), 250, true);
        app.history_open();
        app.overlay_activate();
        assert_eq!(app.overlay_source(), Some(Source::History));
        assert_eq!(app.queue_position(), Some((Source::History, 1, 4)));
        let a = Location::Fs(midi_dir.path().join("a.mid"));
        let step = app.next_in_source(&a).expect("b.mid follows");
        assert_eq!(step.midi.file_name(), "b.mid");
        assert_eq!(step.origin, Origin::History(2));

        // Playing reorders the log, but not the queue.
        app.play_step(step);
        let b = Location::Fs(midi_dir.path().join("b.mid"));
        app.history.record(Some(b.clone()), Some(font), 400, true);
        app.overlay_close();
        assert!(app.next_in_source(&b).is_none(), "closed: nothing follows");
        app.history_open();
        let ov = app.overlay.as_ref().unwrap();
        assert_eq!(ov.state.selected(), Some(2));
        assert_eq!(name_at(ov, 2), "b.mid");
        let step = app.next_in_source(&b).expect("c.mid follows once reopened");
        assert_eq!(step.midi.file_name(), "c.mid");
    }

    fn name_at(ov: &Overlay, i: usize) -> String {
        ov.rows[i].midi.as_ref().unwrap().file_name()
    }

    #[test]
    fn the_playlist_overlay_edits_the_list() {
        let (mut app, _m, sf_dir) = list_app();
        app.playlist_open();
        assert_eq!(app.overlay_source(), Some(Source::Playlist));
        assert_eq!(app.overlay.as_ref().unwrap().rows.len(), 3);

        // `s` needs a loaded font; `x` unpins.
        app.overlay_set_font(true);
        assert!(app.message.as_ref().unwrap().contains("No SoundFont"));
        app.overlay_set_font(false);
        assert_eq!(app.playlist.get(0).unwrap().soundfont, None);
        let two = Location::Fs(sf_dir.path().join("two.sf2"));
        app.soundfont = Some(two.clone());
        app.overlay_set_font(true);
        assert_eq!(app.playlist.get(0).unwrap().soundfont, Some(two));

        app.overlay_move(true);
        assert_eq!(app.playlist.get(1).unwrap().midi.file_name(), "a.mid");
        assert_eq!(app.overlay.as_ref().unwrap().state.selected(), Some(1));

        app.overlay_delete();
        assert_eq!(app.playlist.len(), 2);
        assert!(app.playlist.is_dirty());
    }

    #[test]
    fn adding_from_the_panel_appends_with_or_without_the_font() {
        let (mut app, midi_dir, sf_dir) = list_app();
        app.midi
            .select_loc(&Location::Fs(midi_dir.path().join("b.mid")));
        app.playlist_add(false);
        assert_eq!(app.playlist.len(), 4);
        assert_eq!(app.playlist.get(3).unwrap().soundfont, None);
        // The cursor moved on, so the next press adds the next file.
        assert_eq!(
            app.midi.selected().map(|e| e.name.clone()),
            Some("c.mid".to_string())
        );

        app.playlist_add(true);
        assert!(app.message.as_ref().unwrap().contains("No SoundFont"));
        assert_eq!(app.playlist.len(), 4);
        let one = Location::Fs(sf_dir.path().join("one.sf2"));
        app.soundfont = Some(one.clone());
        app.playlist_add(true);
        assert_eq!(app.playlist.get(4).unwrap().soundfont, Some(one));

        // Only from the MIDI panel.
        app.active = Panel::Sf2;
        app.playlist_add(false);
        assert_eq!(app.playlist.len(), 5);
    }

    #[test]
    fn quitting_with_unsaved_changes_asks_first() {
        let (mut app, _m, _s) = list_app();
        assert!(app.playlist.is_dirty());
        app.request_quit();
        assert!(!app.quit, "the first q only warns");
        app.request_quit();
        assert!(app.quit);

        // Any other key in between takes the question back.
        let (mut app, _m, _s) = list_app();
        app.request_quit();
        app.disarm_discard();
        app.request_quit();
        assert!(!app.quit);

        // Nothing to lose: quit at once.
        let (mut app, _m, _s) = test_app();
        app.request_quit();
        assert!(app.quit);
    }

    #[test]
    fn saving_a_new_playlist_asks_for_a_file_and_guards_others() {
        let (mut app, midi_dir, _s) = list_app();
        app.playlist_open();
        app.overlay_save(false);
        let prompt = app.prompt.as_ref().expect("a new list asks where to save");
        assert_eq!(prompt.kind, PromptKind::SavePlaylist);
        assert!(prompt.buf.ends_with("playlist.m3u"));

        // The extension is added when left off.
        let target = midi_dir.path().join("evening");
        app.prompt.as_mut().unwrap().buf = target.to_string_lossy().into_owned();
        app.prompt_submit();
        assert!(app.prompt.is_none());
        let saved = midi_dir.path().join("evening.m3u");
        assert!(saved.is_file());
        assert!(!app.playlist.is_dirty());
        assert_eq!(app.playlist.path(), Some(&saved));

        // `w` now saves in place, with no prompt.
        app.playlist.remove(0);
        app.overlay_save(false);
        assert!(app.prompt.is_none());
        assert!(!app.playlist.is_dirty());

        // Saving over a different, existing file asks for a second Enter.
        let other = midi_dir.path().join("other.m3u");
        std::fs::write(&other, "keep me\n").unwrap();
        app.overlay_save(true);
        app.prompt.as_mut().unwrap().buf = other.to_string_lossy().into_owned();
        app.prompt_submit();
        assert!(app.prompt.is_some());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep me\n");
        app.prompt_submit();
        assert!(app.prompt.is_none());
        assert!(std::fs::read_to_string(&other)
            .unwrap()
            .starts_with("#EXTM3U"));
    }

    #[test]
    fn enter_on_a_playlist_file_opens_it_and_guards_unsaved_edits() {
        let (mut app, midi_dir, _s) = list_app();
        let file = midi_dir.path().join("list.m3u");
        std::fs::write(&file, "a.mid\n#VOXFONT:sf=x.sf2\n").unwrap();
        app.midi.refresh();
        app.midi.select_loc(&Location::Fs(file.clone()));
        app.active = Panel::Midi;

        // The open list has unsaved edits: the first Enter only asks.
        app.activate_selection();
        assert_eq!(app.playlist.len(), 3);
        assert!(app.overlay.is_none());
        app.activate_selection();
        assert_eq!(app.playlist.len(), 1);
        assert_eq!(app.playlist.path(), Some(&file));
        assert_eq!(app.overlay_source(), Some(Source::Playlist));
        assert!(app.message.as_ref().unwrap().contains("1 line ignored"));
        assert!(app.now_playing.is_none(), "opening does not play");
    }

    /// A test app whose track and font sit one directory below where the
    /// panels start, so following them means the panels change directory.
    fn nested_app() -> (
        App,
        Location,
        Location,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let (mut app, midi_dir, sf_dir) = test_app();
        let msub = midi_dir.path().join("sub");
        let ssub = sf_dir.path().join("fonts");
        std::fs::create_dir(&msub).unwrap();
        std::fs::create_dir(&ssub).unwrap();
        std::fs::write(msub.join("song.mid"), b"x").unwrap();
        std::fs::write(msub.join("next.mid"), b"x").unwrap();
        std::fs::write(ssub.join("piano.sf2"), b"x").unwrap();
        app.midi.refresh();
        app.sf2.refresh();
        let track = Location::Fs(msub.join("song.mid"));
        let font = Location::Fs(ssub.join("piano.sf2"));
        (app, track, font, midi_dir, sf_dir)
    }

    #[test]
    fn playing_the_playlist_leaves_the_panels_where_they_are() {
        let (mut app, track, font, midi_dir, sf_dir) = nested_app();
        let next = Location::Fs(midi_dir.path().join("sub/next.mid"));
        app.playlist.push(track, Some(font.clone()));
        app.playlist.push(next, None);
        app.user_font = Some(font);

        app.play_playlist_at(0);
        assert_eq!(app.queue_position(), Some((Source::Playlist, 1, 2)));
        let step = app.next_playlist_item(list_id(&app, 0), 0).expect("next");
        app.play_step(step);
        assert_eq!(app.queue_position(), Some((Source::Playlist, 2, 2)));

        assert_eq!(
            app.midi.location(),
            Location::Fs(midi_dir.path().to_path_buf())
        );
        assert_eq!(
            app.sf2.location(),
            Location::Fs(sf_dir.path().to_path_buf())
        );
    }

    #[test]
    fn the_history_and_favourites_queues_move_the_panels_onto_each_entry() {
        for origin in [Origin::History(0), Origin::Favourite(0)] {
            let (mut app, track, font, _m, _s) = nested_app();
            app.play_step(Step {
                midi: track.clone(),
                soundfont: Some(font.clone()),
                origin,
            });
            assert_eq!(app.midi.selected().map(|e| e.loc.clone()), Some(track));
            assert_eq!(app.sf2.selected().map(|e| e.loc.clone()), Some(font));
        }
    }
}
