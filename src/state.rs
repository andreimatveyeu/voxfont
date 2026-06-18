//! Persistence of the last session: MIDI directory, SoundFont directory and the
//! loaded SoundFont. Stored as a tiny `key = value` file under the XDG config
//! directory (`$XDG_CONFIG_HOME/voxfont/state.conf`, default `~/.config/...`).

use std::path::PathBuf;

#[derive(Default)]
pub struct State {
    pub midi_dir: Option<PathBuf>,
    pub sf2_dir: Option<PathBuf>,
    pub soundfont: Option<PathBuf>,
}

fn config_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("voxfont"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("voxfont"))
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("state.conf"))
}

/// Pre-rename location (the program used to be called "sfplay"), read as a
/// fallback so existing users keep their saved session.
fn legacy_config_path() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("sfplay").join("state.conf"));
        }
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("sfplay")
            .join("state.conf")
    })
}

pub fn load() -> State {
    let mut state = State::default();
    let text = match config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(t) => t,
        None => match legacy_config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(t) => t,
            None => return state,
        },
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let val = PathBuf::from(val.trim());
            match key.trim() {
                "midi_dir" => state.midi_dir = Some(val),
                "sf2_dir" => state.sf2_dir = Some(val),
                "soundfont" => state.soundfont = Some(val),
                _ => {}
            }
        }
    }
    state
}

pub fn save(state: &State) {
    let path = match config_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut out = String::new();
    let mut line = |key: &str, value: &Option<PathBuf>| {
        if let Some(v) = value {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&v.to_string_lossy());
            out.push('\n');
        }
    };
    line("midi_dir", &state.midi_dir);
    line("sf2_dir", &state.sf2_dir);
    line("soundfont", &state.soundfont);
    let _ = std::fs::write(&path, out);
}
