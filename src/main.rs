mod app;
mod browser;
mod fluid;
mod midi;
mod state;
mod ui;

use app::App;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Hidden diagnostic: `voxfont --selftest <soundfont.sf2> <file.mid>` exercises
    // the fluidsynth FFI path (load/play/pause/seek) without the TUI.
    if args.first().map(|s| s.as_str()) == Some("--selftest") {
        return selftest(args.get(1), args.get(2));
    }

    // Restore the previous session, then apply precedence:
    // CLI arg > saved directory > $HOME.
    let saved = state::load();
    let dir_arg = |a: Option<&String>| a.map(PathBuf::from).filter(|p| p.is_dir());
    let saved_dir = |p: &Option<PathBuf>| p.clone().filter(|p| p.is_dir());
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let midi_dir = dir_arg(args.first())
        .or_else(|| saved_dir(&saved.midi_dir))
        .unwrap_or_else(home);
    let sf2_dir = dir_arg(args.get(1))
        .or_else(|| saved_dir(&saved.sf2_dir))
        .unwrap_or_else(|| midi_dir.clone());

    let mut app = match App::new(midi_dir, sf2_dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("voxfont: failed to initialize: {e}");
            std::process::exit(1);
        }
    };

    // Reload the last SoundFont if it still exists.
    if let Some(sf) = saved.soundfont.filter(|p| p.is_file()) {
        app.load_soundfont(sf);
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
    // Persist the final directories and loaded SoundFont for next launch.
    app.save_state();
    res?;
    Ok(())
}

fn selftest(sf2: Option<&String>, midi: Option<&String>) -> Result<(), Box<dyn std::error::Error>> {
    use std::thread::sleep;
    let sf2 = sf2.ok_or("usage: voxfont --selftest <soundfont.sf2> <file.mid>")?;
    let midi = midi.ok_or("usage: voxfont --selftest <soundfont.sf2> <file.mid>")?;

    let (mut synth, warn) = fluid::Synth::new()?;
    if let Some(w) = warn {
        println!("warning: {w}");
    } else {
        println!("audio driver: OK");
    }

    let p = |s: &str, syn: &fluid::Synth| {
        let (c, t) = syn.position().unwrap_or((-1, -1));
        println!("{s:<22} tick={c:>6}/{t:<6} playing={}", syn.is_playing_status());
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
    synth.play(std::path::Path::new(midi), false)?;
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

        KeyCode::Char('p') | KeyCode::Char(' ') => app.toggle_pause(),
        KeyCode::Char('s') => app.stop(),

        KeyCode::Left => app.seek_seconds(-5),
        KeyCode::Right => app.seek_seconds(5),
        KeyCode::Char('[') => app.seek_seconds(-30),
        KeyCode::Char(']') => app.seek_seconds(30),

        KeyCode::Char('n') => app.next_file(),
        KeyCode::Char('b') => app.prev_file(),

        KeyCode::Char('<') => app.volume_delta(-1),
        KeyCode::Char('>') => app.volume_delta(1),
        KeyCode::Char(',') => app.volume_delta(-5),
        KeyCode::Char('.') => app.volume_delta(5),
        KeyCode::Char(d @ '1'..='9') if alt => app.set_volume((d as u8 - b'0') * 10),

        KeyCode::Char('R') => app.toggle_repeat(),
        KeyCode::Char('X') => app.toggle_auto_next(),
        KeyCode::Char('H') => app.toggle_hidden(),

        // r / Ctrl-r both reload the active panel.
        KeyCode::Char('r') => {
            let _ = ctrl;
            app.active_browser().refresh();
        }

        KeyCode::Char('/') | KeyCode::Char('g') => app.search = Some(String::new()),

        KeyCode::Char('h') | KeyCode::Char('?') => app.show_help = true,

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
        KeyCode::Esc | KeyCode::Enter => app.search = None,
        KeyCode::Backspace => {
            if let Some(q) = app.search.as_mut() {
                q.pop();
            }
            let q = app.search.clone().unwrap_or_default();
            app.active_browser().search(&q);
        }
        KeyCode::Char(c) => {
            if let Some(q) = app.search.as_mut() {
                q.push(c);
            }
            let q = app.search.clone().unwrap_or_default();
            app.active_browser().search(&q);
        }
        _ => {}
    }
}
