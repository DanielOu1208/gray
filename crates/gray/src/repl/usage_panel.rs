use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::SessionTotals;
use crate::setup::ModelRate;

const BAR_CELLS: usize = 24;

pub(crate) enum Warmth {
    Warm(std::time::Duration),
    Cold,
    Unknown,
}

pub(crate) struct PanelInput<'a> {
    pub(crate) totals: &'a SessionTotals,
    pub(crate) model: &'a str,

    pub(crate) rate: Option<ModelRate>,
    pub(crate) warmth: Warmth,
}

pub(crate) fn usage_header(input: &PanelInput<'_>) -> String {
    let n = input.totals.turns;
    format!(
        "{} · {n} turn{}",
        input.model,
        if n == 1 { "" } else { "s" }
    )
}

pub(crate) fn usage_rows(input: &PanelInput<'_>) -> Vec<Line<'static>> {
    let t = input.totals;
    let fl = crate::setup::format_context_length;
    let mut rows = Vec::new();

    let mut tokens = vec![
        body(format!("{} in", fl(t.input))),
        sep(),
        body(format!("{} out", fl(t.output))),
    ];

    if input.rate.is_some() {
        tokens.push(sep());
        tokens.push(body(crate::setup::format_cost(t.cost)));
    }
    rows.push(row("Tokens", tokens));

    if t.cache_reported {
        let fresh = t.input.saturating_sub(t.cache_read + t.cache_write);
        let mut cache = cache_bar(t.cache_read, t.cache_write, fresh);
        cache.push(Span::raw("  "));
        let hit = (t.cache_read as f64 * 100.0 / t.input.max(1) as f64).round() as usize;
        cache.push(styled(
            format!("{}% hit", hit.min(100)),
            crate::theme::theme().cache_hit,
            true,
        ));
        match &input.warmth {
            Warmth::Warm(left) => {
                cache.push(sep());
                cache.push(styled(
                    format!("warm {} left", crate::cache::format_remaining(*left)),
                    crate::theme::theme().success,
                    false,
                ));
            }
            Warmth::Cold => {
                cache.push(sep());
                cache.push(muted("cold"));
            }
            Warmth::Unknown => {}
        }
        rows.push(row("Cache", cache));
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(11)),
            body(format!("{} read", fl(t.cache_read))),
            sep(),
            body(format!("{} written", fl(t.cache_write))),
            sep(),
            body(format!("{} fresh", fl(fresh))),
        ]));

        if let Some(r) = &input.rate
            && r.has_cache_prices
        {
            let saved = if t.saved < 0.0 {
                vec![muted(format!(
                    "−${:.2} (cache writes not yet paid back)",
                    -t.saved
                ))]
            } else {
                let mut v = vec![styled(
                    format!("≈{} vs uncached", crate::setup::format_cost(t.saved)),
                    crate::theme::theme().success,
                    false,
                )];
                if t.write_premium >= 0.01 {
                    v.push(sep());
                    v.push(styled(
                        format!("writes +{}", crate::setup::format_cost(t.write_premium)),
                        crate::theme::theme().success,
                        false,
                    ));
                }
                v
            };
            rows.push(row("Saved", saved));
        }

        let m = &t.misses;
        if m.count == 0 {
            rows.push(row(
                "Misses",
                vec![styled("none", crate::theme::theme().success, false)],
            ));
        } else {
            let mut v = vec![
                styled(m.count.to_string(), crate::theme::theme().error_soft, false),
                sep(),
                styled(
                    format!("{} re-billed", fl(m.tokens)),
                    crate::theme::theme().error_soft,
                    false,
                ),
            ];
            if m.cost >= 0.01 {
                v.push(sep());
                v.push(styled(
                    format!("≈{}", crate::setup::format_cost(m.cost)),
                    crate::theme::theme().error_soft,
                    false,
                ));
            }
            for (n, label) in [
                (m.idle, "idle"),
                (m.model_switch, "model switch"),
                (m.other, "other"),
            ] {
                if n > 0 {
                    v.push(sep());
                    v.push(body(format!("{label} {n}")));
                }
            }
            rows.push(row("Misses", v));
        }
    } else {
        rows.push(row("Cache", vec![muted("provider reports no cache usage")]));
    }

    if t.total_duration_ms > 0 && t.timed_turns > 0 {
        let avg = t.total_duration_ms / t.timed_turns as u64;
        let f = crate::repl::format::fmt_duration_ms;
        rows.push(row(
            "Time",
            vec![
                body(format!("{} total", f(t.total_duration_ms))),
                sep(),
                body(format!("{} avg", f(avg))),
            ],
        ));
    }

    let rate = match &input.rate {
        Some(r) => {
            let mut v = vec![body(format!(
                "{}/{} per 1M in/out",
                crate::setup::format_cost(r.input * 1_000_000.0),
                crate::setup::format_cost(r.output * 1_000_000.0)
            ))];
            if r.has_cache_prices {
                v.push(sep());
                v.push(body(format!(
                    "cached {}",
                    crate::setup::format_cost(r.cache_read * 1_000_000.0)
                )));
            }
            v
        }
        None => vec![muted(
            "unpriced (no rate yet — pricing tables lag new models)",
        )],
    };
    rows.push(row("Rate", rate));

    let spill = gray_core::spill::totals();
    if spill.events > 0 {
        rows.push(row(
            "Squeeze",
            vec![body(format!(
                "{} tool output never sent ({:.0}% of {} produced) · ≈{} tokens",
                gray_core::spill::fmt_bytes(spill.saved() as usize),
                spill.saved_pct(),
                gray_core::spill::fmt_bytes(spill.raw_bytes as usize),
                fl((spill.saved() / 4) as usize),
            ))],
        ));
    }

    rows
}

pub(crate) fn usage_plain(input: &PanelInput<'_>) -> Vec<String> {
    usage_rows(input)
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

pub(crate) fn subscription_plain(limits: &gray_plugin::ProviderUsageLimits) -> Vec<String> {
    subscription_rows(limits)
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn row(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    row_w(label, 9, value)
}

fn row_w(label: &str, width: usize, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![muted(format!("  {label:<width$} "))];
    spans.extend(value);
    Line::from(spans)
}

fn cache_bar(read: usize, write: usize, fresh: usize) -> Vec<Span<'static>> {
    let total = read + write + fresh;
    if total == 0 {
        return Vec::new();
    }
    let parts = [read, write, fresh];
    let mut cells = [0usize; 3];
    for (i, &p) in parts.iter().enumerate() {
        if p > 0 {
            cells[i] = ((p * BAR_CELLS + total / 2) / total).max(1);
        }
    }
    let mut sum: usize = cells.iter().sum();
    while sum > BAR_CELLS {

        let Some(i) = (0..3).filter(|&i| cells[i] > 1).max_by_key(|&i| cells[i]) else {
            break;
        };
        cells[i] -= 1;
        sum -= 1;
    }
    while sum < BAR_CELLS {
        let i = (0..3).max_by_key(|&i| cells[i]).unwrap_or(0);
        cells[i] += 1;
        sum += 1;
    }
    let theme = crate::theme::theme();
    let glyphs = [
        ('█', theme.cache_hit),
        ('▓', theme.accent_soft),
        ('░', theme.text_faint),
    ];
    cells
        .iter()
        .zip(glyphs)
        .filter(|(n, _)| **n > 0)
        .map(|(n, (g, c))| styled(g.to_string().repeat(*n), c, false))
        .collect()
}

fn styled(s: impl Into<String>, color: ratatui::style::Color, bold: bool) -> Span<'static> {
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(s.into(), style)
}

fn body(s: impl Into<String>) -> Span<'static> {
    styled(s, crate::theme::theme().text_body, false)
}

fn muted(s: impl Into<String>) -> Span<'static> {
    styled(s, crate::theme::theme().text_muted, false)
}

fn sep() -> Span<'static> {
    styled(" · ", crate::theme::theme().text_dim, false)
}

fn reset_label(iso: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let now = chrono::Local::now();
    let local = at.with_timezone(&chrono::Local);
    Some(if local.date_naive() == now.date_naive() {
        local.format("%-I:%M%P").to_string()
    } else {
        local.format("%b %-d").to_string()
    })
}

fn fmt_amount(v: f64) -> String {
    if (v - v.round()).abs() < 0.005 {
        format!("{:.0}", v)
    } else {
        format!("{v:.2}")
    }
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "session" => 0,
        "weekly" => 1,
        "monthly" => 2,
        "credits" => 3,
        _ => 4,
    }
}

pub(crate) fn subscription_rows(limits: &gray_plugin::ProviderUsageLimits) -> Vec<Line<'static>> {
    let mut windows: Vec<&gray_plugin::ProviderUsageWindow> = limits.windows.iter().collect();
    windows.sort_by_key(|w| kind_rank(&w.kind));

    let label_w = windows
        .iter()
        .map(|w| w.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(9);
    let mut rows = Vec::new();
    for w in windows {
        let mut v = Vec::new();
        if let Some(pct) = w.used_percent {

            let fill = ((pct / 100.0) * 10.0).round() as usize;
            let color = if pct >= 90.0 {
                crate::theme::theme().error_soft
            } else if pct >= 70.0 {
                crate::theme::theme().rose
            } else {
                crate::theme::theme().cache_hit
            };
            v.push(styled("\u{2588}".repeat(fill.min(10)), color, false));
            v.push(styled(
                "\u{2591}".repeat(10usize.saturating_sub(fill)),
                crate::theme::theme().text_faint,
                false,
            ));
            v.push(styled(format!(" {:>3.0}%", pct), color, true));
        }
        let unit = w
            .unit
            .as_deref()
            .map(|u| format!(" {u}"))
            .unwrap_or_default();

        if w.used_percent.is_none() && w.used.is_none() && w.limit.is_some() {
            v.push(styled(
                "\u{2591}".repeat(10),
                crate::theme::theme().text_faint,
                false,
            ));
        }

        let amount = match (w.used, w.limit) {
            (Some(used), Some(limit)) => Some(format!(
                "{}/{}{}",
                fmt_amount(used),
                fmt_amount(limit),
                unit
            )),
            (None, Some(limit)) => Some(if w.used_percent.is_none() && w.used.is_none() {
                format!("cap {}{}", fmt_amount(limit), unit)
            } else {
                format!("{}{} limit", fmt_amount(limit), unit)
            }),
            (Some(used), None) => Some(format!("{}{}", fmt_amount(used), unit)),
            (None, None) => None,
        };
        if let Some(amount) = amount {
            if !v.is_empty() {
                v.push(sep());
            }
            v.push(body(amount));
        }
        if let Some(label) = w.resets_at.as_deref().and_then(reset_label) {
            v.push(sep());
            v.push(muted(format!("resets {label}")));
        }
        rows.push(row_w(&w.label, label_w, v));
    }
    if let Some(note) = &limits.note {
        rows.push(row_w("", label_w, vec![muted(note.clone())]));
    }
    rows
}

pub(crate) struct UsagePage {
    pub(crate) tab: &'static str,
    pub(crate) title: String,
    pub(crate) lines: Vec<Line<'static>>,
}

pub(crate) fn usage_pages(
    input: &PanelInput<'_>,
    subs: &[(String, gray_plugin::ProviderUsageLimits)],
) -> Vec<UsagePage> {
    let mut pages = Vec::new();
    if !subs.is_empty() {
        let mut lines = Vec::new();
        for (i, (title, limits)) in subs.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            let head = match &limits.plan {
                Some(plan) => format!("{title} {plan}"),
                None => title.clone(),
            };
            lines.push(Line::from(styled(
                head,
                crate::theme::theme().text_body,
                true,
            )));
            lines.extend(subscription_rows(limits));
        }
        pages.push(UsagePage {
            tab: "Usage",
            title: "Usage".into(),
            lines,
        });
    }
    if input.totals.turns > 0 {
        let mut lines = vec![Line::from(styled(
            usage_header(input),
            crate::theme::theme().text_body,
            true,
        ))];
        lines.extend(usage_rows(input));
        pages.push(UsagePage {
            tab: "Session",
            title: "Session usage".into(),
            lines,
        });
    }
    pages
}

pub(crate) fn run_usage_modal(
    rebuild: &mut dyn FnMut() -> Vec<UsagePage>,
    bg: &crate::setup::BackgroundSnapshot,
) -> anyhow::Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
    use ratatui::layout::Rect;
    use ratatui::widgets::{Block, Clear, Paragraph};
    use std::time::Duration;

    let (_session, mut terminal) = crate::setup::open_modal()?;
    let t = crate::theme::theme();
    let (box_bg, accent, text_dim, on_sel) = (t.surface_bg, t.accent, t.text_dim, t.on_selection);
    let mut pages = rebuild();
    let mut page = 0usize;
    let mut scroll = 0usize;

    let out: anyhow::Result<()> = (|| {
        if pages.is_empty() {
            return Ok(());
        }
        loop {
            page = page.min(pages.len().saturating_sub(1));
            terminal.draw(|frame| {
                let area = frame.area();
                if area.width < 30 || area.height < 8 {
                    return;
                }
                crate::setup::render_dimmed_background(frame, bg);
                let modal_w = 88.min(area.width.saturating_sub(2)).max(40).min(area.width);
                let modal_h = ((pages[page].lines.len() as u16) + 4)
                    .min(area.height.saturating_sub(2))
                    .max(8)
                    .min(area.height);
                let modal_rect = Rect::new(
                    (area.width.saturating_sub(modal_w)) / 2,
                    (area.height.saturating_sub(modal_h)) / 4,
                    modal_w,
                    modal_h,
                );
                frame.render_widget(Clear, modal_rect);
                frame.render_widget(
                    Block::default().style(Style::default().bg(box_bg)),
                    modal_rect,
                );
                let inner = Rect::new(
                    modal_rect.x + 2,
                    modal_rect.y + 1,
                    modal_w.saturating_sub(4),
                    modal_h.saturating_sub(2),
                );
                let mut put = |y: u16, line: Line| {
                    frame.render_widget(
                        Paragraph::new(line).style(Style::default().bg(box_bg)),
                        Rect::new(inner.x, inner.y + y, inner.width, 1),
                    );
                };

                let mut tabs: Vec<Span> = Vec::new();
                for (i, p) in pages.iter().enumerate() {
                    if i > 0 {
                        tabs.push(Span::styled("  ", Style::default().bg(box_bg)));
                    }
                    tabs.push(Span::styled(
                        format!(" {} ", p.tab),
                        if i == page {
                            Style::default()
                                .fg(on_sel)
                                .bg(accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(text_dim).bg(box_bg)
                        },
                    ));
                }
                let tabs_w: usize = tabs.iter().map(|s| s.content.chars().count()).sum();
                tabs.push(Span::styled(
                    " ".repeat((inner.width as usize).saturating_sub(tabs_w + 3)),
                    Style::default().bg(box_bg),
                ));
                tabs.push(Span::styled(
                    "esc",
                    Style::default().fg(text_dim).bg(box_bg),
                ));
                put(0, Line::from(tabs));

                let body_h = inner.height.saturating_sub(3) as usize;
                scroll = scroll.min(pages[page].lines.len().saturating_sub(body_h));
                for (i, line) in pages[page]
                    .lines
                    .iter()
                    .skip(scroll)
                    .take(body_h)
                    .enumerate()
                {
                    put(2 + i as u16, line.clone());
                }

                let footer = if pages.len() > 1 {
                    " ←/→ page · ↑↓ scroll · r refresh · esc close"
                } else {
                    " ↑↓ scroll · r refresh · esc close"
                };
                put(
                    inner.height.saturating_sub(1),
                    Line::from(Span::styled(
                        footer,
                        Style::default().fg(text_dim).bg(box_bg),
                    )),
                );
            })?;

            if crate::composer::input::stdin_gone() {
                return Ok(());
            }
            if !poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = read()?
            else {
                continue;
            };
            if kind == KeyEventKind::Release {
                continue;
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                    page = (page + 1) % pages.len();
                    scroll = 0;
                }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                    page = (page + pages.len() - 1) % pages.len();
                    scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    scroll = scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    scroll += 1;
                }
                KeyCode::Char('r') => {
                    pages = rebuild();
                }
                _ => {}
            }
        }
    })();
    out
}

#[path = "usage_panel_tests.rs"]
#[cfg(test)]
mod tests;
