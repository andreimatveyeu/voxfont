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
    let text = config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .or_else(|| legacy_config_path().and_then(|p| std::fs::read_to_string(p).ok()));
    match text {
        Some(t) => parse_conf(&t),
        None => State::default(),
    }
}

pub fn save(state: &State) {
    let path = match config_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, serialize(state));
}

/// Parse the `key = value` config body into a `State`.
fn parse_conf(text: &str) -> State {
    let mut state = State::default();
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

/// Render a `State` to the `key = value` config body (only set fields).
fn serialize(state: &State) -> String {
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serialize_and_parse() {
        let state = State {
            midi_dir: Some(PathBuf::from("/home/me/midi")),
            sf2_dir: Some(PathBuf::from("/srv/sf2")),
            soundfont: Some(PathBuf::from("/srv/sf2/CT8MGM.SF2")),
        };
        let parsed = parse_conf(&serialize(&state));
        assert_eq!(parsed.midi_dir, state.midi_dir);
        assert_eq!(parsed.sf2_dir, state.sf2_dir);
        assert_eq!(parsed.soundfont, state.soundfont);
    }

    #[test]
    fn serialize_omits_unset_fields() {
        let state = State {
            midi_dir: Some(PathBuf::from("/m")),
            ..State::default()
        };
        let out = serialize(&state);
        assert!(out.contains("midi_dir = /m"));
        assert!(!out.contains("sf2_dir"));
        assert!(!out.contains("soundfont"));
    }

    #[test]
    fn parse_tolerates_blanks_comments_and_unknown_keys() {
        let text = "\n# a comment\n  midi_dir =  /a/b \nbogus = x\nsoundfont=/f.sf2\n";
        let s = parse_conf(text);
        assert_eq!(s.midi_dir, Some(PathBuf::from("/a/b")));
        assert_eq!(s.soundfont, Some(PathBuf::from("/f.sf2")));
        assert_eq!(s.sf2_dir, None);
    }

    #[test]
    fn parse_of_empty_is_default() {
        let s = parse_conf("");
        assert!(s.midi_dir.is_none() && s.sf2_dir.is_none() && s.soundfont.is_none());
    }
}
