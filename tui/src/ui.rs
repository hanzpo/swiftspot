use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
    TableState,
};

use crate::state::{Focus, State};

const ACCENT: Color = Color::Green;

pub fn draw(frame: &mut Frame, state: &State) -> Option<Rect> {
    let [body, bar] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).areas(frame.area());
    let [side, main] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(20)]).areas(body);
    draw_side(frame, state, side);
    if state.show_lyrics {
        draw_lyrics(frame, state, main);
    } else {
        draw_main(frame, state, main);
    }
    let art = draw_bar(frame, state, bar);
    if state.help {
        draw_help(frame, body);
    }
    art
}

fn panel(title: String, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), Style::default().bold()))
}

fn selected_style(focused: bool) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn draw_side(frame: &mut Frame, state: &State, area: Rect) {
    let focused = state.focus == Focus::Side;
    let items: Vec<ListItem> = state
        .side
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            let style = if row.target.is_none() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::styled(format!("{indent}{}", row.label), style))
        })
        .collect();
    let list = List::new(items)
        .block(panel("Library".into(), focused))
        .highlight_style(selected_style(focused));
    let mut selection = ListState::default().with_selected(Some(state.side_selected));
    frame.render_stateful_widget(list, area, &mut selection);
}

fn draw_main(frame: &mut Frame, state: &State, area: Rect) {
    let focused = state.focus != Focus::Side;
    let view = &state.view;
    let title = if state.focus == Focus::Input {
        format!("Search: {}_", state.query)
    } else if view.title.is_empty() {
        "Loading".into()
    } else {
        view.title.clone()
    };
    let block = panel(title, focused);
    let message = if !state.signed_in() {
        Some(match state.auth.as_str() {
            "starting" | "connecting" => "Connecting to Spotify…".to_string(),
            _ => "Sign in from the SwiftSpot app.".to_string(),
        })
    } else if let Some(error) = &view.error {
        Some(error.clone())
    } else if view.rows.is_empty() && view.loading {
        Some("Loading…".into())
    } else if view.rows.is_empty() {
        Some("Nothing here.".into())
    } else {
        None
    };
    if let Some(message) = message {
        let text = Paragraph::new(Line::styled(message, Style::default().fg(Color::DarkGray)))
            .alignment(Alignment::Center)
            .block(block);
        frame.render_widget(text, area);
        return;
    }
    let rows: Vec<Row> = view
        .rows
        .iter()
        .map(|row| {
            if row.header {
                return Row::new([Cell::from(Line::styled(
                    row.title.clone(),
                    Style::default().bold().fg(Color::DarkGray),
                ))]);
            }
            let playing = row.uri.is_some() && row.uri.as_deref() == Some(&state.player.uri);
            let title_style = if playing {
                Style::default().fg(ACCENT).bold()
            } else if row.unplayable {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let dim = Style::default().fg(Color::Gray);
            Row::new([
                Cell::from(Span::styled(row.title.clone(), title_style)),
                Cell::from(Span::styled(row.artists.clone(), dim)),
                Cell::from(Span::styled(row.album.clone(), dim)),
                Cell::from(
                    Line::styled(row.duration_ms.map(clock).unwrap_or_default(), dim)
                        .alignment(Alignment::Right),
                ),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(44),
            Constraint::Percentage(28),
            Constraint::Percentage(28),
            Constraint::Length(5),
        ],
    )
    .column_spacing(2)
    .block(block)
    .row_highlight_style(selected_style(focused));
    let mut selection = TableState::default().with_selected(Some(view.selected));
    frame.render_stateful_widget(table, area, &mut selection);
}

fn draw_lyrics(frame: &mut Frame, state: &State, area: Rect) {
    let block = panel("Lyrics".into(), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let dim = Style::default().fg(Color::DarkGray);
    let Some(lyrics) = state
        .lyrics
        .as_ref()
        .filter(|lyrics| lyrics.uri.as_deref() == Some(&state.player.uri) || lyrics.loading)
    else {
        frame.render_widget(
            Paragraph::new(Line::styled("Loading…", dim)).alignment(Alignment::Center),
            inner,
        );
        return;
    };
    if lyrics.lines.is_empty() {
        let message = lyrics
            .error
            .clone()
            .unwrap_or_else(|| "No lyrics for this song.".into());
        frame.render_widget(
            Paragraph::new(Line::styled(message, dim)).alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let position = state.player.position_ms();
    let current = if lyrics.synced {
        lyrics
            .lines
            .iter()
            .rposition(|(time, _)| time.is_some_and(|time| time <= position))
    } else {
        None
    };
    let lines: Vec<Line> = lyrics
        .lines
        .iter()
        .enumerate()
        .map(|(index, (_, text))| {
            let style = match current {
                Some(current) if current == index => Style::default().fg(ACCENT).bold(),
                Some(current) if index < current => dim,
                _ => Style::default(),
            };
            Line::styled(text.clone(), style)
        })
        .collect();
    let half = inner.height / 2;
    let scroll = current.map_or(0, |current| (current as u16).saturating_sub(half));
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .scroll((scroll, 0)),
        inner,
    );
}

fn draw_bar(frame: &mut Frame, state: &State, area: Rect) -> Option<Rect> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let player = &state.player;
    let dim = Style::default().fg(Color::Gray);
    if !player.has_song {
        let text = state.notice().unwrap_or("Nothing playing");
        frame.render_widget(Paragraph::new(Line::styled(text.to_string(), dim)), inner);
        return None;
    }
    let art_width = if state.want_art { inner.height * 2 } else { 0 };
    let [art, _, info] = Layout::horizontal([
        Constraint::Length(art_width),
        Constraint::Length(u16::from(art_width > 0)),
        Constraint::Min(10),
    ])
    .areas(inner);
    if art_width > 0 {
        frame.render_widget(Clear, art);
    }

    let mut title = vec![Span::styled(player.title.clone(), Style::default().bold())];
    if player.saved == Some(true) {
        title.push(Span::styled("  Liked", Style::default().fg(ACCENT)));
    }
    let subtitle = [player.artists.as_str(), player.album.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let third = if let Some(notice) = state.notice() {
        Line::styled(notice.to_string(), Style::default().fg(ACCENT))
    } else {
        let mut parts = Vec::new();
        if let Some(from) = &player.playing_from {
            parts.push(format!("Playing from {from}"));
        }
        if !player.local
            && let Some(device) = &player.device
        {
            parts.push(format!("On {device}"));
        }
        Line::styled(parts.join(" · "), Style::default().fg(Color::DarkGray))
    };

    let position = player.position_ms();
    let flags = flag_spans(state);
    let flags_width: u16 = flags.iter().map(|span| span.width() as u16).sum();
    let elapsed = clock(position);
    let total = clock(player.duration_ms);
    let track = info
        .width
        .saturating_sub(flags_width + elapsed.len() as u16 + total.len() as u16 + 5);
    let filled = (u64::from(track) * position)
        .checked_div(player.duration_ms)
        .map_or(0, |filled| filled.min(u64::from(track)) as usize);
    let mut progress = vec![
        Span::styled(elapsed, dim),
        Span::raw(" "),
        Span::styled("━".repeat(filled), Style::default().fg(ACCENT)),
        Span::styled(
            "─".repeat(usize::from(track).saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
        Span::styled(total, dim),
        Span::raw("   "),
    ];
    progress.extend(flags);

    let lines = vec![
        Line::from(title),
        Line::styled(subtitle, dim),
        third,
        Line::from(progress),
    ];
    frame.render_widget(Paragraph::new(lines), info);
    (art_width > 0).then_some(art)
}

fn flag_spans(state: &State) -> Vec<Span<'static>> {
    let player = &state.player;
    let on = Style::default().fg(ACCENT);
    let off = Style::default().fg(Color::DarkGray);
    let repeat = match player.repeat.as_str() {
        "track" => "Repeat one",
        _ => "Repeat",
    };
    vec![
        Span::styled(if player.playing { "Playing" } else { "Paused" }, off),
        Span::raw("  "),
        Span::styled("Shuffle", if player.shuffle { on } else { off }),
        Span::raw("  "),
        Span::styled(repeat, if player.repeat == "off" { off } else { on }),
        Span::raw("  "),
        Span::styled(format!("{}%", player.volume), off),
    ]
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let keys = [
        ("space", "Play or pause"),
        ("n / p", "Next / previous"),
        ("← / →", "Back / forward 10 seconds"),
        ("+ / -", "Volume"),
        ("s / r", "Shuffle / repeat"),
        ("f", "Like the playing song"),
        ("enter", "Play or open"),
        ("a", "Add to queue"),
        ("/", "Search"),
        ("L", "Lyrics"),
        ("tab, h / l", "Library / list"),
        ("j / k", "Move"),
        ("esc", "Back"),
        ("q", "Quit"),
    ];
    let width = 44.min(area.width);
    let height = (keys.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:>12}  "), Style::default().bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(panel("Keys".into(), true)),
        popup,
    );
}

pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn clock_reads_like_a_player() {
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(3_725_000), "1:02:05");
    }
}
