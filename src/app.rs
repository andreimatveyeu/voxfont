//! Application state and the controller logic that ties the two browser panels
//! to the fluidsynth player.

use crate::browser::Browser;
use crate::fluid::Synth;
use crate::midi::{self, MidiInfo};
use crate::vfs::{self, Location};
use rustyline::completion::{longest_common_prefix, Candidate, FilenameCompleter};
use std::path::PathBuf;
use std::time::Instant;
use tempfile::NamedTempFile;

pub const MIDI_EXTS: &[&str] = &["mid", "midi", "kar", "rmi"];
pub const SF2_EXTS: &[&str] = &["sf2", "sf3"];

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

    /// Jump the active panel's cursor to what it is currently playing/loading:
    /// the playing MIDI file in the MIDI panel, the loaded SoundFont in the
    /// SoundFont panel. Navigates into the right directory or archive first.
    pub fn goto_current(&mut self) {
        let loc = match self.active {
            Panel::Midi => self.now_playing.clone(),
            Panel::Sf2 => self.soundfont.clone(),
        };
        let Some(loc) = loc else {
            self.message = Some(
                match self.active {
                    Panel::Midi => "Nothing playing",
                    Panel::Sf2 => "No SoundFont loaded",
                }
                .into(),
            );
            return;
        };
        self.active_browser().reveal(&loc);
        self.message = None;
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
                self.message = None;
                self.cur_info = midi::parse(&path);
                self.now_playing = Some(loc.clone());
                self.last_played = Some(loc);
                self.play_temp = guard;
                self.state = PlayState::Playing;
                self.play_started = Some(Instant::now());
                self.accumulated_secs = 0.0;
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
            self.play_path(cur); // loop the current track
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
