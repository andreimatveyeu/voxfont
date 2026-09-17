mod app;
mod browser;
mod favourites;
mod fluid;
mod history;
mod midi;
mod state;
mod ui;
mod vfs;

use app::{App, Source};
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
voxfont — console MIDI/SoundFont player

Usage:
    voxfont [-R <driver>] [--no-history] [MIDI_DIR [SOUNDFONT_DIR]]
    voxfont --selftest <soundfont.sf2> <file.mid>
    voxfont -h | --help

Options:
    -R, --driver <driver>   Audio backend: jack (default) or alsa.
        --no-history        Don't read or write the playing history this run.

Positional arguments (both optional):
    MIDI_DIR                Starting directory for the MIDI panel.
    SOUNDFONT_DIR           Starting directory for the SoundFont panel.

When a directory is omitted, the one remembered from the previous session is
used, falling back to $HOME.";

/// Parsed command line. `driver` is `None` unless `-R` was given.
struct Cli {
    driver: Option<String>,
    midi_dir: Option<String>,
    sf2_dir: Option<String>,
    /// `--no-history`: keep the session's plays out of the history file.
    no_history: bool,
}

/// Hand-rolled parser (no external dependency, matching the project's lean
/// dependency policy). `-R/--driver` takes jack|alsa; up to two positionals are
/// the MIDI and SoundFont directories. Returns `Err(message)` on misuse.
fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut driver = None;
    let mut no_history = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-R" | "--driver" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{arg} requires a value (jack or alsa)"))?;
                match val.as_str() {
                    "jack" | "alsa" => driver = Some(val.clone()),
                    other => {
                        return Err(format!("unknown driver '{other}' (expected jack or alsa)"))
                    }
                }
                i += 2;
            }
            // Allow `-R=jack` / `--driver=alsa` too.
            _ if arg.starts_with("-R=") || arg.starts_with("--driver=") => {
                let val = arg.split_once('=').map(|(_, v)| v).unwrap_or("");
                match val {
                    "jack" | "alsa" => driver = Some(val.to_string()),
                    other => {
                        return Err(format!("unknown driver '{other}' (expected jack or alsa)"))
                    }
                }
                i += 1;
            }
            "--no-history" => {
                no_history = true;
                i += 1;
            }
            "--" => {
                positional.extend(args[i + 1..].iter().cloned());
                break;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option '{other}'"));
            }
            _ => {
                positional.push(arg.clone());
                i += 1;
            }
        }
    }
    if positional.len() > 2 {
        return Err(
            "too many positional arguments (expected at most MIDI_DIR and SOUNDFONT_DIR)".into(),
        );
    }
    let mut it = positional.into_iter();
    Ok(Cli {
        driver,
        midi_dir: it.next(),
        sf2_dir: it.next(),
        no_history,
    })
}

/// Turn a directory argument into a `Location`, or `None` (pushing a warning)
/// if it was given but is not a usable directory. `label` names the argument in
/// the warning (e.g. "MIDI_DIR"). A missing argument yields `None` silently.
fn validate_dir(
    arg: Option<&String>,
    label: &str,
    warnings: &mut Vec<String>,
) -> Option<vfs::Location> {
    let p = PathBuf::from(arg?);
    if !p.is_dir() {
        warnings.push(format!("{label} '{}' is not a directory", p.display()));
        None
    } else if std::fs::read_dir(&p).is_err() {
        warnings.push(format!("{label} '{}' is not readable", p.display()));
        None
    } else {
        Some(vfs::Location::Fs(p))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Hidden diagnostic: `voxfont --selftest <soundfont.sf2> <file.mid>` exercises
    // the fluidsynth FFI path (load/play/pause/seek) without the TUI.
    if args.first().map(|s| s.as_str()) == Some("--selftest") {
        return selftest(args.get(1), args.get(2));
    }

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }

    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("voxfont: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Restore the previous session, then apply precedence:
    // CLI arg > saved location > $HOME. A directory passed on the command line
    // that turns out to be unusable is not fatal: we warn (via the status bar)
    // and fall back, rather than silently dropping the argument.
    let saved = state::load();
    let mut warnings: Vec<String> = Vec::new();
    let saved_dir = |l: &Option<vfs::Location>| l.clone().filter(|l| l.is_openable_dir());
    let home = || {
        vfs::Location::Fs(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        )
    };
    let midi_dir = validate_dir(cli.midi_dir.as_ref(), "MIDI_DIR", &mut warnings)
        .or_else(|| saved_dir(&saved.midi_dir))
        .unwrap_or_else(home);
    let sf2_dir = validate_dir(cli.sf2_dir.as_ref(), "SOUNDFONT_DIR", &mut warnings)
        .or_else(|| saved_dir(&saved.sf2_dir))
        .unwrap_or_else(|| midi_dir.clone());

    let mut app = match App::new(midi_dir, sf2_dir, cli.driver.as_deref()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("voxfont: failed to initialize: {e}");
            std::process::exit(1);
        }
    };

    // Fold any directory-argument warnings into the existing message banner
    // (which may already hold an audio-driver warning).
    if !warnings.is_empty() {
        if let Some(m) = app.message.take() {
            warnings.push(m);
        }
        app.message = Some(warnings.join("; "));
    }

    // Load the playing history. `App` starts with an in-memory one that is never
    // written, so `--no-history` (and every test) simply leaves it in place.
    if !cli.no_history {
        app.history = history::History::load();
    }
    // Favourites are always loaded: starring is a deliberate act, not a
    // recording of what happened to play, so `--no-history` does not cover it.
    app.favs = favourites::Favourites::load();

    // Seed the remembered file (before any load, which would re-save state) and
    // put the MIDI cursor on it if it is in the opened directory.
    app.last_played = saved.midi_file.clone();
    if let Some(f) = saved.midi_file.filter(|l| l.exists()) {
        app.midi.select_deep(&f);
    }

    // Reload the last SoundFont if it still exists, landing the cursor on it.
    if let Some(sf) = saved.soundfont.filter(|l| l.exists()) {
        app.sf2.select_deep(&sf);
        app.restore_soundfont(sf);
    }

    // Restore the terminal even if we panic.
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        orig_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let res = run(&mut terminal, &mut app);
    restore_terminal()?;
    // Persist the final directories and loaded SoundFont for next launch, plus
    // whatever was still playing when the user quit.
    app.save_state();
    app.finish();
    res?;
    Ok(())
}

fn selftest(sf2: Option<&String>, midi: Option<&String>) -> Result<(), Box<dyn std::error::Error>> {
    use std::thread::sleep;
    let sf2 = sf2.ok_or("usage: voxfont --selftest <soundfont.sf2> <file.mid>")?;
    let midi = midi.ok_or("usage: voxfont --selftest <soundfont.sf2> <file.mid>")?;

    let (mut synth, warn) = fluid::Synth::new(None)?;
    if let Some(w) = warn {
        println!("warning: {w}");
    } else {
        println!("audio driver: OK");
    }

    let p = |s: &str, syn: &fluid::Synth| {
        let (c, t) = syn.position().unwrap_or((-1, -1));
        println!(
            "{s:<22} tick={c:>6}/{t:<6} playing={}",
            syn.is_playing_status()
        );
    };

    synth.set_gain(0.5);
    synth.load_soundfont(std::path::Path::new(sf2))?;
    println!("loaded soundfont: {sf2}");
    if let Some(info) = midi::parse(std::path::Path::new(midi)) {
        println!(
            "midi: division={} PPQ  timesig={}/{}  duration={:.1}s ({}:{:02})",
            info.division,
            info.ts_num,
            info.ts_den,
            info.duration_secs,
            (info.duration_secs as u64) / 60,
            (info.duration_secs as u64) % 60
        );
    } else {
        println!("midi: parse failed");
    }
    synth.play(std::path::Path::new(midi))?;
    println!("playing: {midi}");

    sleep(Duration::from_millis(1500));
    p("after 1.5s play:", &synth);

    // Seek forward during steady playback (the common case).
    synth.seek_ticks(8000);
    sleep(Duration::from_millis(200));
    p("right after +8000:", &synth);
    sleep(Duration::from_millis(1300));
    p("1.5s after seek:", &synth);

    synth.pause();
    sleep(Duration::from_millis(500));
    p("paused:", &synth);

    synth.resume();
    sleep(Duration::from_millis(1500));
    p("after resume:", &synth);

    synth.stop();
    p("stopped:", &synth);
    println!("selftest OK");
    Ok(())
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(app, f))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key);
                }
            }
        }
        app.tick();
        if app.quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Help overlay swallows the next keypress.
    if app.show_help {
        app.show_help = false;
        return;
    }

    // History / favourites overlay.
    if app.overlay.is_some() {
        handle_overlay_key(app, key);
        return;
    }

    // "Go to directory" prompt.
    if app.goto.is_some() {
        handle_goto_key(app, key);
        return;
    }

    // Incremental search mode.
    if app.search.is_some() {
        handle_search_key(app, key);
        return;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => app.quit = true,

        KeyCode::Tab | KeyCode::BackTab => app.toggle_panel(),

        KeyCode::Up => app.active_browser().move_up(1),
        KeyCode::Down => app.active_browser().move_down(1),
        KeyCode::PageUp => app.active_browser().move_up(10),
        KeyCode::PageDown => app.active_browser().move_down(10),
        KeyCode::Home => app.active_browser().home(),
        KeyCode::End => app.active_browser().end(),

        KeyCode::Enter => app.activate_selection(),
        KeyCode::Char('U') => app.active_browser().go_up(),
        KeyCode::Char('i') => app.start_goto(),
        // Jump to the currently playing MIDI / loaded SoundFont.
        KeyCode::Char('G') => app.goto_current(),
        // Playing history and favourites.
        KeyCode::Char('R') => app.history_open(),
        KeyCode::Char('F') => app.favourites_open(),
        KeyCode::Char('f') => app.toggle_favourite(),

        KeyCode::Char('p') | KeyCode::Char(' ') => app.toggle_pause(),
        KeyCode::Char('s') => app.stop(),

        KeyCode::Left => app.seek_seconds(-5),
        KeyCode::Right => app.seek_seconds(5),
        KeyCode::Char('[') => app.seek_seconds(-30),
        KeyCode::Char(']') => app.seek_seconds(30),

        KeyCode::Char('<') => app.volume_delta(-1),
        KeyCode::Char('>') => app.volume_delta(1),
        KeyCode::Char(',') => app.volume_delta(-5),
        KeyCode::Char('.') => app.volume_delta(5),
        KeyCode::Char(d @ '1'..='9') if alt => app.set_volume((d as u8 - b'0') * 10),

        // Playback modes.
        KeyCode::Char('n') => app.toggle_next_mode(),
        KeyCode::Char('r') if !ctrl => app.toggle_repeat(),
        KeyCode::Char('H') => app.toggle_hidden(),

        // Ctrl-r reloads the active panel.
        KeyCode::Char('r') if ctrl => app.active_browser().refresh(),

        KeyCode::Char('/') | KeyCode::Char('g') => app.search = Some(String::new()),

        KeyCode::Char('h') | KeyCode::Char('?') => app.show_help = true,

        _ => {}
    }
}

/// Keys while the history or favourites overlay is open. `Enter` plays the
/// selected row with its SoundFont; the overlay otherwise navigates like a
/// panel. `R` and `F` switch between the two stores, or close the one showing.
fn handle_overlay_key(app: &mut App, key: KeyEvent) {
    // `/` filter mode captures printable keys; cursor keys still move.
    if app.overlay_filtering() {
        match key.code {
            KeyCode::Esc => app.overlay_filter_cancel(),
            KeyCode::Enter => app.overlay_activate(),
            KeyCode::Backspace => app.overlay_filter_backspace(),
            KeyCode::Char(c) => app.overlay_filter_push(c),
            _ => overlay_move_key(app, key),
        }
        return;
    }

    // Any key other than a second `D` takes back the "erase everything" prompt.
    if key.code != KeyCode::Char('D') {
        app.overlay_cancel_confirm();
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.overlay_close(),
        KeyCode::Char('R') => match app.overlay_source() {
            Some(Source::History) => app.overlay_close(),
            _ => app.history_open(),
        },
        KeyCode::Char('F') => match app.overlay_source() {
            Some(Source::Favourites) => app.overlay_close(),
            _ => app.favourites_open(),
        },
        KeyCode::Enter => app.overlay_activate(),
        KeyCode::Tab | KeyCode::BackTab => app.overlay_cycle_view(),
        KeyCode::Char('G') => app.overlay_reveal(),
        KeyCode::Char('d') => app.overlay_delete(),
        KeyCode::Char('f') => app.overlay_toggle_favourite(),
        KeyCode::Char('D') => app.history_clear(),
        // Reorder the playlist. Shift+arrows are handled in the movement
        // helper, so they work while the filter is capturing keys too; J/K are
        // there for terminals that swallow shifted arrows.
        KeyCode::Char('J') => app.overlay_move(true),
        KeyCode::Char('K') => app.overlay_move(false),
        KeyCode::Char('/') | KeyCode::Char('g') => app.overlay_start_filter(),
        _ => overlay_move_key(app, key),
    }
}

/// Cursor movement inside the overlay, shared by both its modes.
fn overlay_move_key(app: &mut App, key: KeyEvent) {
    // Shifted arrows move the selected favourite, not the cursor.
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            KeyCode::Up => return app.overlay_move(false),
            KeyCode::Down => return app.overlay_move(true),
            _ => {}
        }
    }
    let overlay = match app.overlay.as_mut() {
        Some(o) => o,
        None => return,
    };
    match key.code {
        KeyCode::Up => overlay.move_up(1),
        KeyCode::Down => overlay.move_down(1),
        KeyCode::PageUp => overlay.move_up(10),
        KeyCode::PageDown => overlay.move_down(10),
        KeyCode::Home => overlay.home(),
        KeyCode::End => overlay.end(),
        _ => {}
    }
}

fn handle_goto_key(app: &mut App, key: KeyEvent) {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.goto_cancel(),
        KeyCode::Enter => app.goto_submit(),
        KeyCode::Tab => app.goto_complete(),
        // Alt+Backspace / Ctrl+W: delete the previous path component.
        KeyCode::Backspace if alt => app.goto_delete_component(),
        KeyCode::Char('w') if ctrl => app.goto_delete_component(),
        KeyCode::Backspace => app.goto_backspace(),
        KeyCode::Char(c) => app.goto_push(c),
        _ => {}
    }
}

fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.search_cancel(),
        // Act on the highlighted match and leave the filter.
        KeyCode::Enter => app.search_accept(),
        // Move within the filtered matches without leaving search.
        KeyCode::Up => app.active_browser().move_up(1),
        KeyCode::Down => app.active_browser().move_down(1),
        KeyCode::PageUp => app.active_browser().move_up(10),
        KeyCode::PageDown => app.active_browser().move_down(10),
        KeyCode::Home => app.active_browser().home(),
        KeyCode::End => app.active_browser().end(),
        KeyCode::Backspace => app.search_backspace(),
        KeyCode::Char(c) => app.search_push(c),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_empty_to_all_none() {
        let c = parse_args(&[]).unwrap();
        assert!(c.driver.is_none());
        assert!(c.midi_dir.is_none());
        assert!(c.sf2_dir.is_none());
    }

    #[test]
    fn parses_driver_and_two_dirs() {
        let c = parse_args(&s(&["-R", "alsa", "/midi", "/sf2"])).unwrap();
        assert_eq!(c.driver.as_deref(), Some("alsa"));
        assert_eq!(c.midi_dir.as_deref(), Some("/midi"));
        assert_eq!(c.sf2_dir.as_deref(), Some("/sf2"));
    }

    #[test]
    fn driver_may_follow_positionals_and_use_equals() {
        let c = parse_args(&s(&["/midi", "--driver=jack"])).unwrap();
        assert_eq!(c.driver.as_deref(), Some("jack"));
        assert_eq!(c.midi_dir.as_deref(), Some("/midi"));
        assert!(c.sf2_dir.is_none());
    }

    #[test]
    fn rejects_unknown_driver_and_missing_value() {
        assert!(parse_args(&s(&["-R", "oss"])).is_err());
        assert!(parse_args(&s(&["-R"])).is_err());
        assert!(parse_args(&s(&["--driver=oss"])).is_err());
    }

    #[test]
    fn parses_no_history_flag() {
        let c = parse_args(&s(&["--no-history", "/midi"])).unwrap();
        assert!(c.no_history);
        assert_eq!(c.midi_dir.as_deref(), Some("/midi"));
        assert!(!parse_args(&s(&["/midi"])).unwrap().no_history);
    }

    #[test]
    fn rejects_unknown_option_and_excess_positionals() {
        assert!(parse_args(&s(&["--bogus"])).is_err());
        assert!(parse_args(&s(&["a", "b", "c"])).is_err());
    }

    #[test]
    fn double_dash_forces_positionals() {
        let c = parse_args(&s(&["--", "-R", "x"])).unwrap();
        assert!(c.driver.is_none());
        assert_eq!(c.midi_dir.as_deref(), Some("-R"));
        assert_eq!(c.sf2_dir.as_deref(), Some("x"));
    }

    #[test]
    fn validate_dir_warns_on_missing_path_but_not_on_absent_arg() {
        let mut w = Vec::new();
        assert!(validate_dir(None, "MIDI_DIR", &mut w).is_none());
        assert!(w.is_empty());

        assert!(validate_dir(Some(&"/no/such/dir".to_string()), "MIDI_DIR", &mut w).is_none());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("MIDI_DIR"));
    }

    #[test]
    fn validate_dir_accepts_a_real_directory() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().to_str().unwrap().to_string();
        let mut w = Vec::new();
        let loc = validate_dir(Some(&path), "SOUNDFONT_DIR", &mut w);
        assert!(loc.is_some());
        assert!(w.is_empty());
    }
}
