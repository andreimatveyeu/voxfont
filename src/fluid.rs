//! Thin, hand-written FFI bindings to libfluidsynth (2.x) plus a safe wrapper.
//!
//! Only the small, stable subset of the C API we need is declared. fluidsynth's
//! audio driver runs playback in its own internal thread; we only ever touch the
//! API from the main thread, so no extra synchronization is required here.
#![allow(non_camel_case_types)]

use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_float, c_int, c_uint, c_void};
use std::path::Path;

enum fluid_settings_t {}
enum fluid_synth_t {}
enum fluid_audio_driver_t {}
enum fluid_player_t {}

#[link(name = "fluidsynth")]
extern "C" {
    fn new_fluid_settings() -> *mut fluid_settings_t;
    fn delete_fluid_settings(s: *mut fluid_settings_t);
    fn fluid_settings_setstr(
        s: *mut fluid_settings_t,
        name: *const c_char,
        val: *const c_char,
    ) -> c_int;
    fn fluid_settings_setint(s: *mut fluid_settings_t, name: *const c_char, val: c_int) -> c_int;
    #[allow(dead_code)]
    fn fluid_settings_setnum(s: *mut fluid_settings_t, name: *const c_char, val: c_double)
        -> c_int;

    fn new_fluid_synth(s: *mut fluid_settings_t) -> *mut fluid_synth_t;
    fn delete_fluid_synth(s: *mut fluid_synth_t);
    fn fluid_synth_sfload(
        s: *mut fluid_synth_t,
        filename: *const c_char,
        reset_presets: c_int,
    ) -> c_int;
    fn fluid_synth_sfunload(s: *mut fluid_synth_t, id: c_uint, reset_presets: c_int) -> c_int;
    fn fluid_synth_set_gain(s: *mut fluid_synth_t, gain: c_float);
    fn fluid_synth_system_reset(s: *mut fluid_synth_t) -> c_int;

    fn new_fluid_audio_driver(
        s: *mut fluid_settings_t,
        synth: *mut fluid_synth_t,
    ) -> *mut fluid_audio_driver_t;
    fn delete_fluid_audio_driver(d: *mut fluid_audio_driver_t);

    fn new_fluid_player(synth: *mut fluid_synth_t) -> *mut fluid_player_t;
    fn delete_fluid_player(p: *mut fluid_player_t);
    fn fluid_player_add(p: *mut fluid_player_t, midifile: *const c_char) -> c_int;
    fn fluid_player_play(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_stop(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_seek(p: *mut fluid_player_t, ticks: c_int) -> c_int;
    fn fluid_player_get_status(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_get_current_tick(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_get_total_ticks(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_get_bpm(p: *mut fluid_player_t) -> c_int;
    fn fluid_player_set_loop(p: *mut fluid_player_t, loops: c_int) -> c_int;

    fn fluid_set_log_function(level: c_int, fun: fluid_log_fn, data: *mut c_void) -> *mut c_void;
}

type fluid_log_fn = extern "C" fn(level: c_int, message: *const c_char, data: *mut c_void);

const FLUID_PLAYER_PLAYING: c_int = 1;
const FLUID_PLAYER_DONE: c_int = 3;

/// No-op log sink so fluidsynth never writes to stderr and corrupts the TUI.
extern "C" fn silent_log(_level: c_int, _message: *const c_char, _data: *mut c_void) {}

/// Route all fluidsynth log levels to the no-op sink.
fn silence_fluid_logging() {
    // FLUID_PANIC=0 .. FLUID_DBG=4
    for level in 0..=4 {
        unsafe {
            fluid_set_log_function(level, silent_log, std::ptr::null_mut());
        }
    }
}

fn cstr(p: &Path) -> Result<CString, String> {
    CString::new(p.to_string_lossy().as_bytes()).map_err(|_| "path contains NUL byte".to_string())
}

/// Owns the fluidsynth settings/synth/audio-driver and the current MIDI player.
pub struct Synth {
    settings: *mut fluid_settings_t,
    synth: *mut fluid_synth_t,
    driver: *mut fluid_audio_driver_t,
    player: *mut fluid_player_t,
    sf_id: Option<c_int>,
    paused_tick: c_int,
}

impl Synth {
    /// Build a synth + audio driver. Returns the synth plus an optional warning
    /// (e.g. audio driver failed to start, so playback will be silent).
    ///
    /// `driver` is an explicit backend chosen on the command line (`-R`); when
    /// given it is used verbatim with no fallback. When `None`, the
    /// `VOXFONT_AUDIO_DRIVER` env override wins, otherwise the usual Linux
    /// drivers are tried in order.
    pub fn new(driver: Option<&str>) -> Result<(Synth, Option<String>), String> {
        silence_fluid_logging();
        unsafe {
            let settings = new_fluid_settings();
            if settings.is_null() {
                return Err("new_fluid_settings failed".into());
            }

            // Quiet fluidsynth's own logging so it can't corrupt the TUI.
            if let Ok(k) = CString::new("synth.verbose") {
                fluid_settings_setint(settings, k.as_ptr(), 0);
            }

            // Name the JACK client. PipeWire implements JACK, so when we use the
            // jack driver the graph shows a clean "voxfont" node instead of the
            // ALSA driver's auto-generated "PipeWire ALSA [voxfont]".
            if let Ok(k) = CString::new("audio.jack.id") {
                if let Ok(v) = CString::new("voxfont") {
                    fluid_settings_setstr(settings, k.as_ptr(), v.as_ptr());
                }
            }

            // Autoconnect is opt-in. By default fluidsynth must NOT wire its
            // outputs to the JACK/PipeWire physical playback ports — the user
            // routes them in their patchbay. Set VOXFONT_JACK_AUTOCONNECT=1
            // (or true/yes/on) to let it connect automatically on startup.
            let autoconnect = matches!(
                std::env::var("VOXFONT_JACK_AUTOCONNECT").as_deref(),
                Ok("1") | Ok("true") | Ok("yes") | Ok("on")
            );
            if let Ok(k) = CString::new("audio.jack.autoconnect") {
                fluid_settings_setint(settings, k.as_ptr(), autoconnect as c_int);
            }

            let synth = new_fluid_synth(settings);
            if synth.is_null() {
                delete_fluid_settings(settings);
                return Err("new_fluid_synth failed".into());
            }

            // Pick the audio driver. An explicit `-R` choice wins and is used
            // verbatim (no fallback — the user asked for this backend). Failing
            // that, a VOXFONT_AUDIO_DRIVER env override wins; otherwise prefer
            // JACK (a clean client name on PipeWire/JACK) and fall back to the
            // usual Linux drivers if it isn't available.
            let candidates: Vec<String> = match driver {
                Some(drv) => vec![drv.to_string()],
                None => match std::env::var("VOXFONT_AUDIO_DRIVER") {
                    Ok(drv) => vec![drv],
                    Err(_) => vec!["jack".into(), "pulseaudio".into(), "alsa".into()],
                },
            };
            let mut driver = std::ptr::null_mut();
            for cand in &candidates {
                if let (Ok(k), Ok(v)) = (CString::new("audio.driver"), CString::new(cand.as_str()))
                {
                    fluid_settings_setstr(settings, k.as_ptr(), v.as_ptr());
                }
                driver = new_fluid_audio_driver(settings, synth);
                if !driver.is_null() {
                    break;
                }
            }
            let warning = if driver.is_null() {
                Some("audio driver failed to start — playback will be silent (try VOXFONT_AUDIO_DRIVER=alsa|pulseaudio|pipewire)".to_string())
            } else {
                None
            };

            Ok((
                Synth {
                    settings,
                    synth,
                    driver,
                    player: std::ptr::null_mut(),
                    sf_id: None,
                    paused_tick: 0,
                },
                warning,
            ))
        }
    }

    /// Load a SoundFont, replacing any previously loaded one.
    pub fn load_soundfont(&mut self, path: &Path) -> Result<(), String> {
        let c = cstr(path)?;
        unsafe {
            if let Some(old) = self.sf_id.take() {
                fluid_synth_sfunload(self.synth, old as c_uint, 1);
            }
            let id = fluid_synth_sfload(self.synth, c.as_ptr(), 1);
            if id == -1 {
                return Err(format!("failed to load SoundFont: {}", path.display()));
            }
            self.sf_id = Some(id);
        }
        Ok(())
    }

    pub fn has_soundfont(&self) -> bool {
        self.sf_id.is_some()
    }

    /// Start playing a MIDI file from the beginning. Always plays through once;
    /// repeat / next-track behaviour is handled by the caller on end-of-song.
    pub fn play(&mut self, midi: &Path) -> Result<(), String> {
        let c = cstr(midi)?;
        unsafe {
            self.drop_player();
            // Silence any notes the previous track left held. Stopping/deleting
            // the player does not send note-offs, so without this a new track
            // starts on top of hanging notes. This resets MIDI state on the
            // existing synth only — the audio driver (and its PipeWire/JACK
            // connections) is untouched.
            fluid_synth_system_reset(self.synth);
            let p = new_fluid_player(self.synth);
            if p.is_null() {
                return Err("new_fluid_player failed".into());
            }
            if fluid_player_add(p, c.as_ptr()) != 0 {
                delete_fluid_player(p);
                return Err(format!("cannot load MIDI: {}", midi.display()));
            }
            fluid_player_set_loop(p, 1);
            fluid_player_play(p);
            self.player = p;
            self.paused_tick = 0;
        }
        Ok(())
    }

    /// Pause: remember the position and stop the player.
    pub fn pause(&mut self) {
        if self.player.is_null() {
            return;
        }
        unsafe {
            self.paused_tick = fluid_player_get_current_tick(self.player);
            fluid_player_stop(self.player);
            fluid_synth_system_reset(self.synth);
        }
    }

    /// Resume from the remembered position.
    pub fn resume(&mut self) {
        if self.player.is_null() {
            return;
        }
        unsafe {
            fluid_player_play(self.player);
            fluid_player_seek(self.player, self.paused_tick);
        }
    }

    /// Stop and rewind to the start, silencing any hanging notes.
    pub fn stop(&mut self) {
        if self.player.is_null() {
            return;
        }
        unsafe {
            fluid_player_stop(self.player);
            fluid_player_seek(self.player, 0);
            fluid_synth_system_reset(self.synth);
        }
        self.paused_tick = 0;
    }

    /// Seek by a signed delta in ticks, clamped to the song bounds.
    pub fn seek_ticks(&mut self, delta: i32) {
        if self.player.is_null() {
            return;
        }
        unsafe {
            let cur = fluid_player_get_current_tick(self.player);
            let total = fluid_player_get_total_ticks(self.player);
            let mut target = cur + delta;
            if target < 0 {
                target = 0;
            }
            if total > 0 && target > total {
                target = total;
            }
            fluid_player_seek(self.player, target);
        }
    }

    /// gain: 0.0 (silent) .. ~1.0 (default fluidsynth gain is 0.2).
    pub fn set_gain(&mut self, gain: f32) {
        unsafe {
            fluid_synth_set_gain(self.synth, gain);
        }
    }

    /// (current_tick, total_tick) for the active player, if any.
    pub fn position(&self) -> Option<(i32, i32)> {
        if self.player.is_null() {
            return None;
        }
        unsafe {
            Some((
                fluid_player_get_current_tick(self.player),
                fluid_player_get_total_ticks(self.player),
            ))
        }
    }

    /// Current playback tempo in beats per minute, if a player is loaded.
    pub fn bpm(&self) -> Option<i32> {
        if self.player.is_null() {
            return None;
        }
        unsafe { Some(fluid_player_get_bpm(self.player)) }
    }

    /// True while the player's internal status reports active playback.
    pub fn is_playing_status(&self) -> bool {
        if self.player.is_null() {
            return false;
        }
        unsafe { fluid_player_get_status(self.player) == FLUID_PLAYER_PLAYING }
    }

    /// True once the current song has played to its end (status DONE). This is
    /// the reliable end-of-song signal — the tick counter can stop short of, or
    /// overshoot, the reported total, so don't rely on it for completion.
    pub fn is_finished(&self) -> bool {
        if self.player.is_null() {
            return false;
        }
        unsafe { fluid_player_get_status(self.player) == FLUID_PLAYER_DONE }
    }

    unsafe fn drop_player(&mut self) {
        if !self.player.is_null() {
            fluid_player_stop(self.player);
            delete_fluid_player(self.player);
            self.player = std::ptr::null_mut();
        }
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        unsafe {
            self.drop_player();
            if !self.driver.is_null() {
                delete_fluid_audio_driver(self.driver);
            }
            if !self.synth.is_null() {
                delete_fluid_synth(self.synth);
            }
            if !self.settings.is_null() {
                delete_fluid_settings(self.settings);
            }
        }
    }
}
