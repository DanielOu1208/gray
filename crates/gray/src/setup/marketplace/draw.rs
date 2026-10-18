use gray_pkg::index::Entry;
use gray_pkg::staged::InstallStage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::ConsentView;
use super::model::{MarketModel, MarketPhase};

pub(super) fn safe_text(raw: &str) -> String {
    let plain = crate::composer::input::strip_escape_sequences(raw);
    plain
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(|word| {
                    let word: String = word.chars().filter(|c| !c.is_control()).collect();
                    if word.contains("://") {
                        gray_pkg::fetch::redact(&word)
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn trust(entry: &Entry) -> &'static str {
    if entry.official {
        "official"
    } else if entry.verified {
        "verified community"
    } else {
        "unverified community"
    }
}

pub fn status_text(model: &MarketModel) -> Option<&str> {
    match model.phase {
        MarketPhase::Loading => Some("Loading marketplace…"),
        MarketPhase::LoadError => model.message.as_deref(),
        MarketPhase::Browsing if model.entries.is_empty() => Some("No plugins available"),
        MarketPhase::Browsing if model.visible_names().is_empty() => Some("No matching plugins"),
        _ => None,
    }
}

pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &MarketModel,
    consent: Option<&ConsentView>,
) {
    if area.width < 12 || area.height < 3 {
        frame.render_widget(Paragraph::new("Resize to browse"), area);
        return;
    }
    let theme = crate::theme::theme();
    let style = Style::default().fg(theme.text_bright).bg(theme.surface_bg);
    let footer = match model.phase {
        MarketPhase::Loading => "",
        MarketPhase::LoadError => "[ Retry: r ]  [ Back: Esc ]",
        MarketPhase::Browsing if model.search_focused => "Type to search · Enter/Esc done",
        MarketPhase::Browsing if model.details => {
            if model.can_install() {
                "[ Install ] i/Enter · Esc back · ↑↓ scroll"
            } else {
                "[ Install disabled ] · Esc back · ↑↓ scroll"
            }
        }
        MarketPhase::Browsing => "/ search · ↑↓/jk select · Enter details · i install",
        MarketPhase::Confirm => "[ Approve: Enter ]  [ Back: Esc ]",
        MarketPhase::Preparing => "Esc cancel · cleanup finishes before leaving",
        MarketPhase::Consent if model.grant_selected => "> [ Grant: Enter/g ]  [ Cancel: Esc ]",
        MarketPhase::Consent => "[ Grant: g ]  > [ Cancel: Enter/Esc ] · Tab select",
        MarketPhase::Committing => "Finishing installation · cancellation disabled",
        MarketPhase::Installed => "[ Back: Enter/Esc ]",
        MarketPhase::InstallFailed => "[ Retry: r ]  [ Back: Esc ]",
    };
    let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(theme.text_dim).bg(theme.surface_bg)),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );
    if let Some(status) = status_text(model) {
        let heading = if model.phase == MarketPhase::LoadError {
            format!("Marketplace unavailable\n{}", safe_text(status))
        } else {
            status.into()
        };
        frame.render_widget(
            Paragraph::new(heading)
                .style(style)
                .wrap(Wrap { trim: false }),
            body,
        );
        return;
    }
    if model.phase == MarketPhase::Browsing && !model.details {
        let marker = if model.search_focused { "> " } else { "" };
        frame.render_widget(
            Paragraph::new(format!("{marker}Search: {}", safe_text(&model.query))).style(style),
            Rect::new(body.x, body.y, body.width, 1),
        );
        let items = model
            .visible_names()
            .into_iter()
            .map(|name| {
                let entry = &model.entries[name];
                let badge = if model.installed.contains(name) {
                    " [installed]"
                } else {
                    ""
                };
                ListItem::new(format!("{} · {}{badge}", safe_text(name), trust(entry)))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default()
            .with_selected(Some(model.selected))
            .with_offset(model.offset);
        frame.render_stateful_widget(
            List::new(items)
                .style(style)
                .highlight_symbol("> ")
                .highlight_style(
                    Style::default()
                        .fg(theme.on_selection)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            Rect::new(
                body.x,
                body.y.saturating_add(1),
                body.width,
                body.height.saturating_sub(1),
            ),
            &mut state,
        );
        return;
    }

    let text = match model.phase {
        MarketPhase::Browsing => details(model),
        MarketPhase::Confirm => model.approved.as_ref().map(|approved| {
            let entry = approved.entry();
            format!(
                "Confirm preparation: {}\nVersion: {}\nSource: {}\nHash: {}\nTrust: {}\n\nA matching hash proves integrity, not safety.\nPreparing will run this plugin's manifest command as you.",
                approved.name(), entry.version, gray_pkg::fetch::redact(&entry.source.url),
                entry.hash.primary().unwrap_or("—"), trust(entry)
            )
        }).unwrap_or_default(),
        MarketPhase::Preparing | MarketPhase::Committing => {
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
            let stage = match model.stage {
                Some(InstallStage::Downloading) => "Downloading",
                Some(InstallStage::Unpacking) => "Unpacking",
                Some(InstallStage::Scanning) => "Scanning",
                Some(InstallStage::ReadingManifest) => "Reading manifest",
                Some(InstallStage::Committing) => "Committing",
                None => "Preparing",
            };
            format!("{} {}\n{}", spinner[model.spinner % spinner.len()], stage,
                if model.cancelling || model.close_requested { "Waiting for cleanup or commit to finish…" } else { "" })
        }
        MarketPhase::Consent => consent_text(consent),
        MarketPhase::Installed => format!("Installed\n{}", model.message.as_deref().unwrap_or_default()),
        MarketPhase::InstallFailed => format!("Install failed\n{}", model.message.as_deref().unwrap_or_default()),
        MarketPhase::Loading | MarketPhase::LoadError => String::new(),
    };
    let text = safe_text(&text);
    let lines = wrapped_lines(&text, body.width);
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let paragraph = Paragraph::new(lines)
        .style(style)
        .wrap(Wrap { trim: false });
    // Clamp the view, not just input, so scrolling cannot hide all content.
    frame.render_widget(
        paragraph.scroll((
            usize::from(model.scroll)
                .min(max_scroll)
                .min(u16::MAX as usize) as u16,
            0,
        )),
        body,
    );
}

fn wrapped_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut used = 0;
        for c in line.chars() {
            let w = c.width().unwrap_or_default();
            if used + w > usize::from(width) && !current.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            current.push(c);
            used += w;
        }
        lines.push(Line::from(current));
    }
    lines
}

fn details(model: &MarketModel) -> String {
    let Some(name) = model.selected_name() else {
        return String::new();
    };
    let entry = &model.entries[name];
    let mut lines = vec![
        format!(
            "{name}{}",
            if model.installed.contains(name) {
                " [installed]"
            } else {
                ""
            }
        ),
        entry.description.clone().unwrap_or_default(),
        format!("Author: {}", entry.author.as_deref().unwrap_or("—")),
        format!("Version: {}", entry.version),
        format!("Trust: {}", trust(entry)),
        format!(
            "Kind: {}",
            if entry.kind.is_empty() {
                "—"
            } else {
                &entry.kind
            }
        ),
        format!(
            "Commands: {}",
            if entry.commands.is_empty() {
                "—".into()
            } else {
                entry.commands.join(", ")
            }
        ),
        format!(
            "License: {}",
            if entry.license.is_empty() {
                "—"
            } else {
                &entry.license
            }
        ),
        format!("Source: {}", gray_pkg::fetch::redact(&entry.source.url)),
    ];
    if let Some(url) = &entry.homepage {
        lines.push(format!("Homepage: {}", gray_pkg::fetch::redact(url)));
    }
    if let Some(url) = &entry.repo {
        lines.push(format!("Repo: {}", gray_pkg::fetch::redact(url)));
    }
    if !model.can_install() && !model.installed.contains(name) {
        lines.push("This catalog package is not supported by the marketplace.".into());
    }
    lines.join("\n")
}

fn consent_text(consent: Option<&ConsentView>) -> String {
    let Some(consent) = consent else {
        return String::new();
    };
    let mut lines = vec!["Capability consent".into(), "Capabilities:".into()];
    if consent.capabilities.is_empty() {
        lines.push("None declared".into());
    }
    for capability in &consent.capabilities {
        lines.push(format!(
            "{}{}: {}",
            capability.id,
            if capability.grantable {
                ""
            } else {
                " [not grantable]"
            },
            capability.description
        ));
    }
    lines.push(format!(
        "Commands: {}",
        if consent.commands.is_empty() {
            "None declared".into()
        } else {
            consent.commands.join(", ")
        }
    ));
    lines.push("Scan:".into());
    lines.push(consent.scan_text.clone());
    lines.push("Grant accepts the displayed scan findings and known capabilities.".into());
    lines.join("\n")
}
