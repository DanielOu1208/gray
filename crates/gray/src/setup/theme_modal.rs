use super::*;
use crate::theme::{self, UiTheme};

struct Row {
    name: String,
    theme: UiTheme,
}

fn rows() -> Vec<Row> {
    let mut out = vec![Row {
        name: theme::BUILTIN_THEME_NAME.to_string(),
        theme: theme::GRAY_UI_THEME,
    }];
    if let Ok(dir) = theme::themes_dir() {
        for name in theme::list_themes_in(&dir) {
            if theme::is_builtin_name(&name) {
                continue;
            }
            if let Ok((t, _)) = theme::load_theme_in(&dir, &name) {
                out.push(Row { name, theme: t });
            }
        }
    }
    out
}

fn apply(row: &Row) {
    theme::set_theme(
        row.theme,
        (!theme::is_builtin_name(&row.name)).then_some(row.name.as_str()),
    );
}

fn first_match<'a>(rows: &'a [Row], query: &str) -> Option<&'a Row> {
    let q = query.to_lowercase();
    rows.iter().find(|r| r.name.to_lowercase().contains(&q))
}

pub(crate) fn run_theme_picker(bg: Option<&BackgroundSnapshot>) -> anyhow::Result<Option<String>> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
    use std::time::Duration;

    let rows = rows();
    if rows.len() <= 1 {
        return Ok(None);
    }
    let (_session, mut terminal) = super::open_modal()?;
    let bg_snapshot = bg
        .cloned()
        .unwrap_or_else(BackgroundSnapshot::default_initial);
    let active = theme::active_theme_name();
    let mut sel = rows
        .iter()
        .position(|r| {
            active.as_deref() == Some(r.name.as_str())
                || (active.is_none() && theme::is_builtin_name(&r.name))
        })
        .unwrap_or(0);
    let orig_theme = theme::theme();
    let orig_name = active.clone();
    let mut query = String::new();

    (|| -> anyhow::Result<Option<String>> {
        loop {
            let q = query.to_lowercase();
            let filtered: Vec<&Row> = rows
                .iter()
                .filter(|r| q.is_empty() || r.name.to_lowercase().contains(&q))
                .collect();
            sel = sel.min(filtered.len().saturating_sub(1));
            let list_h = filtered.len().clamp(1, 12);

            terminal.draw(|frame| {
                let th = theme::theme();
                let area = frame.area();
                if area.width < 24 || area.height < 8 {
                    return;
                }
                render_dimmed_background(frame, &bg_snapshot);
                let inner_h = (list_h + 5) as u16;
                let modal_w = 44.min(area.width.saturating_sub(2));
                let modal_h = (inner_h + 2).min(area.height.saturating_sub(2));
                let modal_x = (area.width.saturating_sub(modal_w)) / 2;
                let modal_y = (area.height.saturating_sub(modal_h)) / 3;
                let modal = Rect::new(modal_x, modal_y, modal_w, modal_h);
                frame.render_widget(Clear, modal);
                let counter = if filtered.is_empty() {
                    " 0/0 ".to_string()
                } else {
                    format!(" {}/{} ", sel + 1, filtered.len())
                };
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(th.text_faint))
                    .style(Style::default().bg(th.surface_bg))
                    .title(Span::styled(" theme ", Style::default().fg(th.text_muted)))
                    .title_bottom(
                        Line::from(Span::styled(counter, Style::default().fg(th.text_muted)))
                            .right_aligned(),
                    );
                let inner = block.inner(modal);
                frame.render_widget(block, modal);
                let inner_bottom = inner.y + inner.height;

                let mut y = inner.y;
                if y < inner_bottom {
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(format!(":{query}"), Style::default().fg(th.text_body)),
                            Span::styled("█", Style::default().fg(th.text_body)),
                        ])),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                }

                let list_y = inner.y + 2;
                let start = sel
                    .saturating_sub(list_h.saturating_sub(1))
                    .min(filtered.len().saturating_sub(list_h));
                if filtered.is_empty() {
                    if list_y < inner_bottom {
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                "  no match",
                                Style::default().fg(th.text_dim),
                            ))),
                            Rect::new(inner.x, list_y, inner.width, 1),
                        );
                    }
                } else {
                    for (vi, row) in filtered.iter().enumerate().skip(start).take(list_h) {
                        y = list_y + (vi - start) as u16;
                        if y >= inner_bottom {
                            break;
                        }
                        let is_sel = vi == sel;
                        let swatch = theme::swatch_spans(&row.theme);
                        let swatch_w: usize =
                            swatch.iter().map(|s| s.content.chars().count()).sum();
                        let marker = if is_sel { "› " } else { "  " };
                        let left_w = 2 + row.name.chars().count();
                        let pad = (inner.width as usize).saturating_sub(left_w + swatch_w + 1);
                        let mut spans = vec![
                            Span::styled(
                                marker,
                                Style::default().fg(if is_sel { th.accent } else { th.text_dim }),
                            ),
                            Span::styled(
                                row.name.clone(),
                                if is_sel {
                                    Style::default()
                                        .fg(th.text_bright)
                                        .add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default().fg(th.text_dim)
                                },
                            ),
                            Span::raw(" ".repeat(pad)),
                        ];
                        spans.extend(swatch);
                        frame.render_widget(
                            Paragraph::new(Line::from(spans)),
                            Rect::new(inner.x, y, inner.width, 1),
                        );
                    }
                }

                let spec_y = list_y + list_h as u16 + 1;
                let specimen: Vec<Line> = vec![
                    Line::from(vec![
                        Span::styled("Aa bright", Style::default().fg(th.text_bright)),
                        Span::styled("  body text", Style::default().fg(th.text_body)),
                        Span::styled("  dim", Style::default().fg(th.text_dim)),
                        Span::styled("  faint", Style::default().fg(th.text_faint)),
                    ]),
                    Line::from(vec![
                        Span::styled("✓ success", Style::default().fg(th.success)),
                        Span::styled("  ✗ error", Style::default().fg(th.error)),
                        Span::styled("  i info", Style::default().fg(th.info)),
                        Span::styled("  accent", Style::default().fg(th.accent)),
                        Span::styled("  rose", Style::default().fg(th.rose)),
                    ]),
                ];
                for (i, line) in specimen.into_iter().enumerate() {
                    let y = spec_y + i as u16;
                    if y >= inner_bottom {
                        break;
                    }
                    frame
                        .render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
                }
            })?;

            if !poll(Duration::from_millis(100))? {
                continue;
            }
            match read()? {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => match code {
                    KeyCode::Char('c') => {
                        theme::set_theme(orig_theme, orig_name.as_deref());
                        return Ok(None);
                    }
                    KeyCode::Char('p') => {
                        let next = sel.saturating_sub(1);
                        if next != sel {
                            sel = next;
                            if let Some(row) = filtered.get(sel) {
                                apply(row);
                            }
                        }
                    }
                    KeyCode::Char('n') => {
                        let next = (sel + 1).min(filtered.len().saturating_sub(1));
                        if next != sel {
                            sel = next;
                            if let Some(row) = filtered.get(sel) {
                                apply(row);
                            }
                        }
                    }
                    _ => {}
                },
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) => {
                    let last = filtered.len().saturating_sub(1);
                    let next = match code {
                        KeyCode::Up => Some(sel.saturating_sub(1)),
                        KeyCode::Down => Some((sel + 1).min(last)),
                        KeyCode::PageUp => Some(sel.saturating_sub(list_h)),
                        KeyCode::PageDown => Some((sel + list_h).min(last)),
                        KeyCode::Home => Some(0),
                        KeyCode::End => Some(last),
                        _ => None,
                    };
                    if let Some(n) = next {
                        if n != sel {
                            sel = n;
                            if let Some(row) = filtered.get(sel) {
                                apply(row);
                            }
                        }
                        continue;
                    }
                    match code {
                        KeyCode::Enter => {
                            if let Some(row) = filtered.get(sel) {
                                return Ok(Some(row.name.clone()));
                            }
                        }
                        KeyCode::Esc => {
                            if query.is_empty() {
                                theme::set_theme(orig_theme, orig_name.as_deref());
                                return Ok(None);
                            }
                            query.clear();
                            sel = 0;
                            if let Some(row) = rows.first() {
                                apply(row);
                            }
                        }
                        KeyCode::Backspace => {
                            if query.pop().is_some() {
                                sel = 0;
                                if let Some(row) = first_match(&rows, &query) {
                                    apply(row);
                                }
                            }
                        }
                        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::ALT) => {
                            query.push(c);
                            sel = 0;
                            if let Some(row) = first_match(&rows, &query) {
                                apply(row);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    })()
}
