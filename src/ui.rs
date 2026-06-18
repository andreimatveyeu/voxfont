//! ratatui rendering: two browser panels above a player bar.

use crate::app::{App, Panel, PlayState};
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
            } else {
                ("   ", Style::default().fg(Color::White))
            };

            let style = if playing {
                base.fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                base
            };
            let marker = if playing { "♪ " } else { "  " };

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

    let ratio = app.progress();
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(state_col).bg(Color::Black))
        .ratio(ratio)
        .label(format!("{:.0}%", ratio * 100.0));
    frame.render_widget(gauge, rows[1]);
}

fn draw_hints(app: &App, frame: &mut Frame, area: Rect) {
    // When the GO prompt is open it takes over this line.
    if let Some(path) = &app.goto {
        let line = Line::from(vec![
            Span::styled(
                "GO: ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{path}█"), Style::default().fg(Color::Cyan)),
            Span::styled(
                "   (Tab: complete  Alt+⌫: up  Enter: go  Esc: cancel)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let hint = "Tab panels  Enter play/load  Space pause  s stop  ←/→ seek  n next-mode  r repeat  </> vol  i go  G playing  / filter  h help  q quit";
    frame.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        area,
    );
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
    marker: &'a str,
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
