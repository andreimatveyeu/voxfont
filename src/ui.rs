//! ratatui rendering: two browser panels above a player bar.

use crate::app::{App, Panel, PlayState, PromptKind, Source};
use crate::history;
use crate::playlist;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // panels
            Constraint::Length(4), // player bar
            Constraint::Length(1), // key hints
        ])
        .split(area);

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    draw_panel(f, frame, panels[0], Panel::Midi, "MIDI files");
    draw_panel(f, frame, panels[1], Panel::Sf2, "SoundFonts");
    draw_player(f, frame, rows[1]);
    draw_hints(f, frame, rows[2]);

    // An overlay stays open while its list plays, so it keeps to the panels
    // and leaves the player bar and the key hints in view.
    if f.overlay.is_some() {
        draw_overlay(f, frame, rows[0]);
    }
    if f.show_help {
        draw_help(frame, area);
    }
}

fn draw_panel(app: &mut App, frame: &mut Frame, area: Rect, panel: Panel, title: &str) {
    let active = app.active == panel;
    let browser = match panel {
        Panel::Midi => &app.midi,
        Panel::Sf2 => &app.sf2,
    };

    let border_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let dir = browser.dir_display();
    let n = browser.item_count();
    // Item count sits on the bottom border (right-aligned) instead of taking a
    // whole row of its own. While filtering it shows matches out of the total.
    let count_text = if browser.is_filtered() {
        format!(" {n} of {} ", browser.total_count())
    } else {
        format!(" {n} item{} ", if n == 1 { "" } else { "s" })
    };
    let count = Line::from(Span::styled(
        count_text,
        Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
    ))
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            format!(" {title} — {dir} "),
            Style::default().fg(if active { Color::Cyan } else { Color::Gray }),
        ))
        .title_bottom(count);

    // Inner width available for one row (panel width minus the two borders).
    let inner_w = area.width.saturating_sub(2) as usize;

    let items: Vec<ListItem> = browser
        .entries
        .iter()
        .map(|e| {
            let playing = app
                .now_playing
                .as_ref()
                .map(|p| p == &e.loc)
                .unwrap_or(false)
                || app.soundfont.as_ref().map(|p| p == &e.loc).unwrap_or(false);

            let (icon, base) = if e.is_parent {
                ("..", Style::default().fg(Color::Yellow))
            } else if e.is_dir {
                (
                    "[+]",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                )
            } else if panel == Panel::Midi && playlist::is_playlist_name(&e.name) {
                // A playlist opens rather than plays.
                (" ≡ ", Style::default().fg(Color::Magenta))
            } else {
                ("   ", Style::default().fg(Color::White))
            };

            let style = if playing {
                base.fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                base
            };
            // Two columns: what is playing, and the favourite star — solid when
            // this exact pair is starred, hollow when the item is starred in
            // some other pairing.
            let marker = format!(
                "{}{}",
                if playing { "♪" } else { " " },
                app.star_for(panel, &e.loc)
            );

            // Right-hand column: archive/SoundFont size or MIDI duration.
            // Archives are shown as directories but carry a file size, rendered
            // the same way as SoundFont sizes; plain directories stay blank.
            let right = if e.is_dir {
                if e.size > 0 {
                    human_size(e.size)
                } else {
                    String::new()
                }
            } else {
                match panel {
                    Panel::Sf2 => human_size(e.size),
                    Panel::Midi => e.duration.map(fmt_hms).unwrap_or_default(),
                }
            };

            ListItem::new(row_line(marker, icon, &e.name, style, &right, inner_w))
        })
        .collect();

    let highlight = if active {
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray)
    };

    // No highlight symbol: keeps the right-hand column aligned on every row.
    let list = List::default()
        .items(items)
        .block(block)
        .highlight_style(highlight);

    // ListState needs &mut, so render against the concrete browser.
    let state = match panel {
        Panel::Midi => &mut app.midi.state,
        Panel::Sf2 => &mut app.sf2.state,
    };
    frame.render_stateful_widget(list, area, state);
}

fn draw_player(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Player ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let (state_icon, state_col) = match app.state {
        PlayState::Playing => ("▶ PLAY ", Color::Green),
        PlayState::Paused => ("⏸ PAUSE", Color::Yellow),
        PlayState::Stopped => ("⏹ STOP ", Color::Red),
    };

    let track = app
        .now_playing
        .as_ref()
        .map(|p| p.file_name())
        .unwrap_or_else(|| "—".to_string());
    let sf = app
        .soundfont
        .as_ref()
        .map(|p| p.file_name())
        .unwrap_or_else(|| "none".to_string());

    let (elapsed, total) = app.times();

    let mut spans = vec![
        Span::styled(
            state_icon,
            Style::default().fg(state_col).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            track,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("[{} / {}]", fmt_time(elapsed), fmt_time(total)),
            Style::default().fg(Color::Gray),
        ),
    ];

    // Bar:beat · time signature · tempo (only while a track is loaded).
    if let Some((bar, beat)) = app.bar_beat() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("♪ {bar}:{beat}"),
            Style::default().fg(Color::Cyan),
        ));
        if let Some((n, d)) = app.time_signature() {
            spans.push(Span::styled(
                format!(" {n}/{d}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if let Some(bpm) = app.bpm() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{bpm} BPM"),
            Style::default().fg(Color::Green),
        ));
    }

    spans.extend([
        Span::raw("   "),
        Span::styled(format!("SF: {sf}"), Style::default().fg(Color::Magenta)),
        Span::raw("   "),
        Span::styled(
            format!("Vol {:>3}%", app.volume),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    // Playback-mode badges: lit when the mode is on, dim when off.
    let on = Style::default()
        .fg(Color::Black)
        .bg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let off = Style::default().fg(Color::DarkGray);
    spans.push(Span::raw("   "));
    spans.push(Span::styled(" Next ", if app.next_mode { on } else { off }));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(" Rep ", if app.repeat { on } else { off }));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        " Fav ",
        if app.current_is_favourite() { on } else { off },
    ));
    // While the favourites or the playlist are the queue, show how far along.
    // A list only plays on while its overlay is open; closed, its badge dims
    // like a mode that is off.
    if let Some((source, at, n)) = app.queue_position() {
        let label = match source {
            Source::History => "Hist",
            Source::Favourites => "Favs",
            Source::Playlist => "List",
        };
        let style = match app.queue_live() {
            false => off,
            true => Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {label} {at}/{n} "), style));
    }

    if let Some(msg) = &app.message {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(q) = &app.search {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("/{q}"),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    let (ratio, gauge_col) = gauge_appearance(app.state, app.progress(), state_col);
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(gauge_col).bg(Color::Black))
        .ratio(ratio)
        .label(format!("{:.0}%", ratio * 100.0));
    frame.render_widget(gauge, rows[1]);
}

/// Ratio and colour for the playback progress bar. When stopped the bar is
/// reset to empty (the player keeps reporting its last tick, so we don't trust
/// `progress()` here) and shown in a neutral colour rather than recoloured red.
fn gauge_appearance(state: PlayState, progress: f64, state_col: Color) -> (f64, Color) {
    match state {
        PlayState::Stopped => (0.0, Color::DarkGray),
        _ => (progress, state_col),
    }
}

fn draw_hints(app: &App, frame: &mut Frame, area: Rect) {
    // When the path prompt is open it takes over this line.
    if let Some(prompt) = &app.prompt {
        let (label, keys) = match prompt.kind {
            PromptKind::Goto => (
                "GO: ",
                "   (Tab: complete  Alt+⌫: up  Enter: go  Esc: cancel)",
            ),
            PromptKind::SavePlaylist => (
                "SAVE PLAYLIST: ",
                "   (Tab: complete  Enter: save  Esc: cancel)",
            ),
        };
        let line = Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{}█", prompt.buf), Style::default().fg(Color::Cyan)),
            Span::styled(keys, Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    // The overlays leave this line in view; say which keys still reach the
    // player from inside one.
    if app.overlay.is_some() {
        let hint = "Enter play  Space pause  s stop  ←/→ seek  n next-mode  r repeat  </> vol  Esc close — the current track plays out";
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            area,
        );
        return;
    }
    let hint = "Tab panels  Enter play/load  Space pause  s stop  ←/→ seek  n next-mode  r repeat  </> vol  i go  G playing  R history  f star  F favs  P playlist  a add  / filter  h help  q quit";
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        area,
    );
}

/// The overlay, showing either store as a table of (track, SoundFont) rows.
///
/// For the history that is what was played, through which font and when, newest
/// first, with `Tab` switching between the combination view and the per-track /
/// per-SoundFont groupings of the same log. For the favourites it is the starred
/// pairs in playlist order, and for the playlist its items in order; in both
/// `Shift`+`↑`/`↓` reorders.
fn draw_overlay(app: &mut App, frame: &mut Frame, area: Rect) {
    let (source, view, filter, filtering, count) = match &app.overlay {
        Some(o) => (
            o.source,
            o.view,
            o.filter.clone(),
            o.filtering,
            o.rows.len(),
        ),
        None => return,
    };
    let total = match source {
        Source::History => app.history.len(),
        Source::Favourites => app.favs.len(),
        Source::Playlist => app.playlist.len(),
    };

    let w = area.width.saturating_sub(4).min(110);
    // Tall enough for the rows it has (headings + list + detail + borders),
    // without covering more of the panels than it needs to — and never taller
    // than the space they have, however small that is.
    let max_h = area.height.saturating_sub(2);
    let h = (count as u16 + 5).max(6.min(max_h)).min(max_h);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, popup);

    let heading = match source {
        Source::History => format!("History — {}", view.title()),
        Source::Favourites => "Favourites".to_string(),
        Source::Playlist => format!("Playlist — {}", app.playlist.name()),
    };
    // Saving the playlist is explicit, so say when there is something to save.
    let modified = if source == Source::Playlist && app.playlist.is_dirty() {
        " · modified"
    } else {
        ""
    };
    // While filtering the count reads "matches of total", as the panels do.
    let title = if filtering || !filter.is_empty() {
        format!(" {heading} · /{filter} · {count} of {total}{modified} ")
    } else {
        format!(" {heading} · {count}{modified} ")
    };
    let hint = match source {
        Source::History => {
            " Enter play from here · Tab view · f star · G reveal · d forget · D erase all · / filter "
        }
        Source::Favourites => {
            " Enter play from here · ⇧↑↓ move · d remove · G reveal · / filter "
        }
        Source::Playlist => {
            " Enter play from here · ⇧↑↓ move · S set font · x clear font · d remove · w save · W save as · / filter "
        }
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // column headings
            Constraint::Min(1),    // the list
            Constraint::Length(2), // full paths of the selected row
        ])
        .split(inner);

    let cols = history_columns(inner.width, source);
    let when_heading = match source {
        Source::History => "when",
        Source::Favourites => "starred",
        Source::Playlist => "     #",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            history_row_text(when_heading, "track", "soundfont", "plays", cols),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[0],
    );

    let now = history::now_secs();
    // Precomputed here rather than per row: the favourites overlay marks the
    // row that is playing, the history overlay marks the rows that are starred.
    let playing = (app.now_playing.clone(), app.soundfont.clone());
    // A playlist may hold the same pair twice, so its playing row is the one
    // at the queue's position, not every row that matches what is heard.
    let playing_item = match app.queue_position() {
        Some((Source::Playlist, at, _)) => Some(at - 1),
        _ => None,
    };
    let items: Vec<ListItem> = app
        .overlay
        .as_ref()
        .map(|ov| {
            ov.rows
                .iter()
                .map(|r| {
                    // A row whose file has since disappeared is dimmed and
                    // flagged, rather than silently failing when played.
                    let gone = r.gone;
                    let is_playing = !gone
                        && match source {
                            Source::Playlist => playing_item == r.idxs.first().copied(),
                            _ => (r.midi.clone(), r.soundfont.clone()) == playing,
                        };
                    let style = match (gone, is_playing) {
                        (true, _) => Style::default().fg(Color::DarkGray),
                        (false, true) => Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        _ => Style::default().fg(Color::White),
                    };
                    // Two mark columns: gone, then the row's own state. Every
                    // favourite is starred by definition, so there that slot
                    // shows what is playing instead.
                    let state = match (source, is_playing) {
                        (Source::Favourites, playing) => {
                            if playing {
                                "♪"
                            } else {
                                " "
                            }
                        }
                        (Source::Playlist, true) => "♪",
                        // An item without a font is starred through the
                        // font it plays through.
                        (Source::Playlist, false) => {
                            match (&r.midi, r.soundfont.as_ref().or(app.user_font.as_ref())) {
                                (Some(m), Some(sf)) if app.favs.is_pair(m, sf) => "★",
                                _ => " ",
                            }
                        }
                        (Source::History, _) => match (&r.midi, &r.soundfont) {
                            (Some(m), Some(sf)) if app.favs.is_pair(m, sf) => "★",
                            _ => " ",
                        },
                    };
                    let mark = format!("{}{}", if gone { "!" } else { " " }, state);
                    // A starred pair that has never been played has no count.
                    let plays = match r.plays {
                        0 => String::new(),
                        n => format!("{n}x"),
                    };
                    // The first column is the time for the stores that keep
                    // one, and the item's number in the playlist.
                    let first = match source {
                        Source::Playlist => format!("{:>4}", r.idxs[0] + 1),
                        _ => history::fmt_stamp(r.when, false),
                    };
                    let font = match (source, &r.soundfont) {
                        (Source::Playlist, None) => "(your font)".to_string(),
                        _ => name_of(&r.soundfont),
                    };
                    ListItem::new(Line::from(Span::styled(
                        history_row_text(
                            &format!("{mark}{first}"),
                            &name_of(&r.midi),
                            &font,
                            &plays,
                            cols,
                        ),
                        style,
                    )))
                })
                .collect()
        })
        .unwrap_or_default();

    let list = List::default().items(items).highlight_style(
        Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    if let Some(ov) = app.overlay.as_mut() {
        frame.render_stateful_widget(list, rows[1], &mut ov.state);
    }

    // Detail: the full locations of the selected row, which the columns above
    // can only show the names of.
    let detail: Vec<Line> = match app.overlay.as_ref().and_then(|o| o.selected()) {
        Some(r) if source == Source::Playlist => {
            let font = match (&r.soundfont, &app.user_font) {
                (Some(_), _) => detail_line("font ", &r.soundfont, ""),
                (None, Some(_)) => detail_line("font ", &app.user_font, "your font"),
                (None, None) => detail_line("font ", &None, "your font — none loaded yet"),
            };
            vec![detail_line("track", &r.midi, ""), font]
        }
        Some(r) => {
            let age = history::fmt_age(now, r.when);
            vec![
                detail_line("track", &r.midi, &age),
                detail_line("font ", &r.soundfont, ""),
            ]
        }
        None => vec![Line::from(Span::styled(
            match source {
                Source::History => "  nothing played yet — press Enter on a MIDI file to start",
                Source::Favourites => "  nothing starred yet — press f while a track is playing",
                Source::Playlist => {
                    "  empty — press a on a MIDI file to add it, A to add it with the loaded SoundFont"
                }
            },
            Style::default().fg(Color::DarkGray),
        ))],
    };
    frame.render_widget(Paragraph::new(detail), rows[2]);
}

/// Column widths of the overlay table: timestamp (or item number), track,
/// soundfont, plays.
fn history_columns(width: u16, source: Source) -> (usize, usize, usize, usize) {
    let stamp = match source {
        Source::Playlist => 6, // "!♪ 123"
        _ => 18,               // "!★YYYY-MM-DD HH:MM"
    };
    let plays = 5;
    let rest = (width as usize).saturating_sub(stamp + plays + 4); // 4 = gaps
    let track = rest / 2;
    (stamp, track, rest.saturating_sub(track), plays)
}

/// One history table row, padded into the given columns.
fn history_row_text(
    when: &str,
    track: &str,
    font: &str,
    plays: &str,
    cols: (usize, usize, usize, usize),
) -> String {
    let (w_when, w_track, w_font, w_plays) = cols;
    format!(
        "{} {} {} {:>w$}",
        truncate_pad(when, w_when),
        truncate_pad(track, w_track),
        truncate_pad(font, w_font),
        plays,
        w = w_plays
    )
}

fn name_of(loc: &Option<crate::vfs::Location>) -> String {
    loc.as_ref()
        .map(|l| l.file_name())
        .unwrap_or_else(|| "—".to_string())
}

fn detail_line<'a>(label: &'a str, loc: &Option<crate::vfs::Location>, suffix: &str) -> Line<'a> {
    let path = loc
        .as_ref()
        .map(|l| l.display())
        .unwrap_or_else(|| "—".to_string());
    let mut spans = vec![
        Span::styled(format!(" {label}  "), Style::default().fg(Color::DarkGray)),
        Span::styled(path, Style::default().fg(Color::Gray)),
    ];
    if !suffix.is_empty() {
        spans.push(Span::styled(
            format!("   ({suffix})"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(Span::styled(
            "voxfont — keyboard shortcuts",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Tab            switch between MIDI / SoundFont panels"),
        Line::from("  ↑ ↓ PgUp PgDn  move cursor    Home/End  first/last"),
        Line::from("  Enter          enter dir · play MIDI · load SoundFont"),
        Line::from("  U              go up a directory"),
        Line::from("  i              go to directory (Tab completes)"),
        Line::from("  G              jump to playing track / loaded SoundFont"),
        Line::from("  R              playing history — Enter plays it from there"),
        Line::from("  f              star the track + SoundFont being heard"),
        Line::from("  F              favourites — Enter plays the list from there"),
        Line::from("  P              playlist — Enter plays it from there, w saves"),
        Line::from("                 R/F/P lists play on only while they are open"),
        Line::from("  a / A          add MIDI file to the playlist (A: with loaded font)"),
        Line::from("  Space / p      pause / resume"),
        Line::from("  s              stop"),
        Line::from("  ← →            seek 5s    [ ]  seek 30s"),
        Line::from("  n              next mode: auto-play next file when done"),
        Line::from("  r              repeat mode: loop the track or directory"),
        Line::from("  < >            volume -1 / +1     , .  volume -5 / +5"),
        Line::from("  M-1..M-9       volume 10%..90%"),
        Line::from("  H              toggle hidden files   ^r  reload panel"),
        Line::from("  / or g         filter list  (↑↓ move · Enter act · Esc clear)"),
        Line::from("  h / ?          this help          q / Q  quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let w = 60u16.min(area.width.saturating_sub(2));
    let h = (text.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Help ");
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "00:00".to_string();
    }
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Duration label for the file list: m:ss, or h:mm:ss past an hour.
fn fmt_hms(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return String::new();
    }
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Human-readable byte size (e.g. "6.4M", "275M").
fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < UNITS.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else if f < 10.0 {
        format!("{:.1}{}", f, UNITS[i])
    } else {
        format!("{:.0}{}", f, UNITS[i])
    }
}

/// Build one list row: a 2-col marker, the "icon name" left field, and a
/// right-aligned column (size/duration). Columns stay aligned because no
/// highlight symbol shifts the rows.
fn row_line<'a>(
    marker: String,
    icon: &str,
    name: &str,
    name_style: Style,
    right: &str,
    inner_w: usize,
) -> Line<'a> {
    let left = format!("{icon} {name}");
    if right.is_empty() {
        return Line::from(vec![Span::raw(marker), Span::styled(left, name_style)]);
    }
    let right_w = right.chars().count();
    // Budget for the left field = width − marker(2) − right − gap(1).
    let avail = inner_w.saturating_sub(2 + right_w + 1);
    Line::from(vec![
        Span::raw(marker),
        Span::styled(truncate_pad(&left, avail), name_style),
        Span::raw(" "),
        Span::styled(right.to_string(), Style::default().fg(Color::DarkGray)),
    ])
}

/// Truncate (with an ellipsis) or right-pad `s` to exactly `w` display columns.
fn truncate_pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if w == 0 {
        return String::new();
    }
    if len <= w {
        format!("{s:<w$}")
    } else {
        let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the whole UI into an off-screen terminal of the given size and
    /// return its text, so layout and content can be asserted headlessly.
    fn rendered(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn history_overlay_shows_the_pair_and_its_timestamp() {
        use crate::vfs::Location;
        use std::path::PathBuf;

        let midi_dir = tempfile::tempdir().unwrap();
        let sf_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Location::Fs(midi_dir.path().to_path_buf()),
            Location::Fs(sf_dir.path().to_path_buf()),
            None,
        )
        .expect("app init");

        let when = 1_758_067_400;
        app.history.record(
            Some(Location::Fs(PathBuf::from("/m/CANYON.MID"))),
            Some(Location::Fs(PathBuf::from("/s/CT8MGM.SF2"))),
            when,
            true,
        );
        app.history_open();

        let out = rendered(&mut app, 100, 24);
        assert!(out.contains("History"), "{out}");
        assert!(out.contains("CANYON.MID"), "{out}");
        assert!(out.contains("CT8MGM.SF2"), "{out}");
        // The absolute time is on the row, not just a relative age.
        assert!(out.contains(&history::fmt_stamp(when, false)), "{out}");
        // The detail lines spell out where each file actually lives.
        assert!(out.contains("/m/CANYON.MID"), "{out}");

        // A cramped terminal must still render rather than panic on layout.
        let _ = rendered(&mut app, 24, 6);
    }

    #[test]
    fn favourites_show_a_star_in_the_panels_and_list_in_the_overlay() {
        use crate::vfs::Location;

        let midi_dir = tempfile::tempdir().unwrap();
        let sf_dir = tempfile::tempdir().unwrap();
        let track = midi_dir.path().join("CANYON.MID");
        let font = sf_dir.path().join("CT8MGM.SF2");
        std::fs::write(&track, b"x").unwrap();
        std::fs::write(&font, b"x").unwrap();

        let mut app = App::new(
            Location::Fs(midi_dir.path().to_path_buf()),
            Location::Fs(sf_dir.path().to_path_buf()),
            None,
        )
        .expect("app init");
        app.midi.refresh();
        app.sf2.refresh();
        app.now_playing = Some(Location::Fs(track.clone()));
        app.soundfont = Some(Location::Fs(font.clone()));
        app.favs
            .toggle(Location::Fs(track), Location::Fs(font), 1_758_067_400);

        // Both panels mark the starred pair solid — the track in one, the font
        // it is starred with in the other.
        let out = rendered(&mut app, 100, 24);
        assert_eq!(out.matches('★').count(), 2, "one star per panel:\n{out}");
        // And the player bar's Fav badge is there.
        assert!(out.contains("Fav"), "{out}");

        // The overlay lists the pair, under its own heading and hints.
        app.favourites_open();
        let out = rendered(&mut app, 100, 24);
        assert!(out.contains("Favourites"), "{out}");
        assert!(out.contains("CANYON.MID"), "{out}");
        assert!(out.contains("CT8MGM.SF2"), "{out}");
        assert!(out.contains("Enter play from here"), "{out}");
        // A star on every row would be noise, so the overlay adds none — the
        // two still on screen are the panels' own, behind the popup — and the
        // slot marks the playing row instead, the third ♪ here.
        assert_eq!(out.matches('★').count(), 2, "{out}");
        assert_eq!(out.matches('♪').count(), 3, "{out}");

        // A cramped terminal must still render rather than panic on layout.
        let _ = rendered(&mut app, 24, 6);
    }

    #[test]
    fn the_playlist_shows_in_the_panel_and_overlay() {
        use crate::vfs::Location;

        let midi_dir = tempfile::tempdir().unwrap();
        let sf_dir = tempfile::tempdir().unwrap();
        let track = midi_dir.path().join("CANYON.MID");
        let font = sf_dir.path().join("CT8MGM.SF2");
        std::fs::write(&track, b"x").unwrap();
        std::fs::write(&font, b"x").unwrap();
        std::fs::write(midi_dir.path().join("evening.m3u"), b"").unwrap();

        let mut app = App::new(
            Location::Fs(midi_dir.path().to_path_buf()),
            Location::Fs(sf_dir.path().to_path_buf()),
            None,
        )
        .expect("app init");
        // The playlist file is listed with its own marker.
        let out = rendered(&mut app, 100, 24);
        assert!(out.contains(" ≡  evening.m3u"), "{out}");

        app.playlist
            .push(Location::Fs(track.clone()), Some(Location::Fs(font)));
        app.playlist.push(Location::Fs(track), None);
        app.playlist_open();
        let out = rendered(&mut app, 110, 24);
        assert!(out.contains("Playlist — untitled · 2 · modified"), "{out}");
        assert!(out.contains("CT8MGM.SF2"), "{out}");
        assert!(out.contains("(your font)"), "{out}");
        assert!(out.contains("S set font"), "{out}");
        // The first column numbers the items.
        assert!(out.contains("     1 CANYON.MID"), "{out}");

        // The overlay keeps to the panels: the player bar and the hints, which
        // now list the keys that reach the player, stay in view.
        assert!(out.contains(" Player "), "{out}");
        assert!(out.contains("Space pause"), "{out}");
        assert!(out.contains("s stop"), "{out}");

        // The save prompt takes over the bottom line.
        app.overlay_save(false);
        let out = rendered(&mut app, 110, 24);
        assert!(out.contains("SAVE PLAYLIST: "), "{out}");

        let _ = rendered(&mut app, 24, 6);
    }

    #[test]
    fn stopped_resets_gauge_instead_of_recolouring() {
        // While playing/paused the bar keeps its progress and state colour.
        assert_eq!(
            gauge_appearance(PlayState::Playing, 0.42, Color::Green),
            (0.42, Color::Green)
        );
        assert_eq!(
            gauge_appearance(PlayState::Paused, 0.42, Color::Yellow),
            (0.42, Color::Yellow)
        );
        // On stop the bar resets to empty and is not recoloured red, even though
        // the synth still reports a near-full position.
        assert_eq!(
            gauge_appearance(PlayState::Stopped, 0.99, Color::Red),
            (0.0, Color::DarkGray)
        );
    }

    #[test]
    fn fmt_time_minutes_and_hours() {
        assert_eq!(fmt_time(0.0), "00:00");
        assert_eq!(fmt_time(-3.0), "00:00");
        assert_eq!(fmt_time(7.0), "00:07");
        assert_eq!(fmt_time(98.0), "01:38");
        assert_eq!(fmt_time(3661.0), "1:01:01");
        assert_eq!(fmt_time(f64::NAN), "00:00");
    }

    #[test]
    fn fmt_hms_for_file_list() {
        assert_eq!(fmt_hms(0.0), "");
        assert_eq!(fmt_hms(128.0), "2:08");
        assert_eq!(fmt_hms(89.6), "1:30"); // rounds
        assert_eq!(fmt_hms(3725.0), "1:02:05");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2 * 1024 + 100), "2.1K");
        assert_eq!(human_size(8 * 1024 * 1024), "8.0M");
        assert_eq!(human_size(148 * 1024 * 1024), "148M");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn truncate_pad_pads_and_truncates() {
        assert_eq!(truncate_pad("abc", 5), "abc  ");
        assert_eq!(truncate_pad("abc", 3), "abc");
        assert_eq!(truncate_pad("abcdef", 4), "abc…");
        assert_eq!(truncate_pad("anything", 0), "");
        // Result width is always exactly `w` columns.
        assert_eq!(truncate_pad("hello world", 6).chars().count(), 6);
        assert_eq!(truncate_pad("hi", 6).chars().count(), 6);
    }
}
