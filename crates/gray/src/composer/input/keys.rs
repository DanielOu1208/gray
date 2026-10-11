//! One key dispatcher for both composer input loops.
//!
//! The idle prompt (`read_line`) and the mid-turn key watcher used to
//! carry their own copies of every editing key and had drifted apart
//! (mid-turn Tab skipped `completion_fill`, Ctrl+D typed a `d`). Both now
//! resolve the key through [`crate::keymap`] and apply it here; only the
//! outcomes that differ by context (submit, interrupt, clear, exit, a
//! bound slash command) go back to the caller.

use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::Tui;
use crate::keymap::{Action, Binding};

/// What the caller has to do after [`dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyOutcome {
    /// Draft, cursor or popup changed: redraw (and refresh completions).
    Edited,
    /// Nothing bound or applicable.
    Ignored,
    /// Send the draft (idle) or queue it (mid-turn).
    Submit,
    /// `app.clear`: clear the draft, exit on an empty prompt, or cancel.
    Clear,
    /// `app.exit` on an empty draft.
    Exit,
    /// `app.interrupt`: cancel the turn / clear the draft.
    Interrupt,
    /// A slash command bound in `keybindings.json`.
    Command(String),
}

/// Resolves `ev` against the active keymap and applies editor actions.
pub(crate) fn dispatch(tui: &mut Tui, ev: &KeyEvent) -> KeyOutcome {
    let bindings = crate::keymap::resolve_event(ev);
    dispatch_bindings(tui, ev, &bindings)
}

static CHIP_MODE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_chip_sel(tui: &mut Tui, sel: Option<usize>) {
    tui.chip_sel = sel;
    CHIP_MODE.store(sel.is_some(), Ordering::Relaxed);
}

pub(crate) fn chip_mode_hint() -> bool {
    CHIP_MODE.load(Ordering::Relaxed)
}

fn chip_key(tui: &mut Tui, bindings: &[Binding]) -> Option<KeyOutcome> {
    let sel = tui.chip_sel?;
    if tui.attachments.is_empty() {
        set_chip_sel(tui, None);
        return None;
    }
    let last = tui.attachments.len() - 1;
    let sel = sel.min(last);
    for b in bindings {
        let Binding::Action(a) = b else {
            continue;
        };
        match a {
            Action::CursorLeft => set_chip_sel(tui, Some(sel.saturating_sub(1))),
            Action::CursorRight => set_chip_sel(tui, Some((sel + 1).min(last))),
            Action::SelectUp | Action::CursorUp | Action::HistoryPrevious => {
                set_chip_sel(tui, Some(sel))
            }
            Action::DeleteCharBackward | Action::DeleteCharForward => remove_chip(tui, sel),
            Action::SelectDown
            | Action::CursorDown
            | Action::HistoryNext
            | Action::SelectCancel
            | Action::Interrupt => set_chip_sel(tui, None),
            _ => continue,
        }
        return Some(KeyOutcome::Edited);
    }
    set_chip_sel(tui, None);
    None
}

fn remove_chip(tui: &mut Tui, idx: usize) {
    let (ph, _) = tui.attachments.remove(idx);
    let text = tui.textarea.text().to_string();
    if let Some(start) = text.find(&ph) {
        let mut end = start + ph.len();
        let bytes = text.as_bytes();
        if start > 0 && bytes[start - 1] == b' ' && bytes.get(end) == Some(&b' ') {
            end += 1;
        }
        tui.textarea.replace_range(start..end, "");
    }
    super::sync_attachments(tui);
    if tui.attachments.is_empty() {
        set_chip_sel(tui, None);
    } else {
        let next = idx.saturating_sub(1).min(tui.attachments.len() - 1);
        set_chip_sel(tui, Some(next));
    }
}

fn enter_chip_mode(tui: &mut Tui) -> bool {
    if tui.attachments.is_empty() || !tui.matches.is_empty() {
        return false;
    }
    let text = tui.textarea.text();
    let cur = tui.textarea.cursor().min(text.len());
    if text[..cur].contains('\n') {
        return false;
    }
    let last = tui.attachments.len() - 1;
    set_chip_sel(tui, Some(last));
    true
}

pub(crate) fn dispatch_bindings(tui: &mut Tui, ev: &KeyEvent, bindings: &[Binding]) -> KeyOutcome {
    if let Some(out) = chip_key(tui, bindings) {
        return out;
    }
    for b in bindings {
        match b {
            Binding::Command(cmd) => return KeyOutcome::Command(cmd.clone()),
            Binding::Action(a) => {
                if let Some(out) = apply(tui, *a) {
                    return out;
                }
            }
        }
    }
    // Unbound printable key: type it. Ctrl/Alt chords never insert.
    if let KeyCode::Char(c) = ev.code
        && !ev
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        tui.textarea.insert_str(&c.to_string());
        tui.history_idx = None;
        tui.sel = 0;
        return KeyOutcome::Edited;
    }
    KeyOutcome::Ignored
}

/// Applies one action; `None` means "not applicable here, try the next
/// action bound to this key" (Up with no popup, Ctrl+D on a draft).
fn apply(tui: &mut Tui, a: Action) -> Option<KeyOutcome> {
    use KeyOutcome::*;
    let popup = !tui.matches.is_empty();
    let multi = tui.matches.len() > 1;
    let edited = Some(Edited);
    match a {
        Action::SelectUp => {
            if !multi {
                return None;
            }
            tui.sel = tui.sel.saturating_sub(1);
            edited
        }
        Action::SelectDown => {
            if !multi {
                return None;
            }
            tui.sel = (tui.sel + 1).min(tui.matches.len().saturating_sub(1));
            edited
        }
        Action::SelectCancel => {
            if !popup {
                return None;
            }
            tui.matches.clear();
            tui.sel = 0;
            edited
        }
        Action::Tab => {
            let (name, _) = tui.matches.get(tui.sel)?.clone();
            fill_completion(tui, &name);
            edited
        }
        Action::Clear => Some(Clear),
        Action::Exit => tui.textarea.is_empty().then_some(Exit),
        Action::Interrupt => Some(Interrupt),
        Action::PasteClipboard => {
            super::paste_from_system_clipboard(tui);
            tui.sel = 0;
            edited
        }
        Action::NewLine => {
            tui.textarea.insert_str("\n");
            edited
        }
        Action::Submit => {
            // An open popup completes its selection first; a second Enter
            // on the completed command sends it.
            if let Some((name, _)) = tui.matches.get(tui.sel).cloned() {
                let cur = tui.textarea.text();
                if cur != format!("/{name}") && cur != format!("/{name} ") {
                    fill_completion(tui, &name);
                    return edited;
                }
            }
            Some(Submit)
        }
        Action::HistoryPrevious => {
            if !enter_chip_mode(tui) {
                history_prev(tui);
            }
            edited
        }
        Action::HistoryNext => {
            history_next(tui);
            edited
        }
        Action::CursorUp => {
            if enter_chip_mode(tui) {
                return edited;
            }
            let has_multiline = tui.textarea.text().contains('\n');
            let at_top = tui.textarea.cursor() == 0 || !has_multiline;
            if at_top && !tui.history.is_empty() {
                history_prev(tui);
            } else {
                tui.textarea.move_up();
            }
            edited
        }
        Action::CursorDown => {
            if tui.history_idx.is_some() {
                history_next(tui);
            } else {
                tui.textarea.move_down();
            }
            edited
        }
        Action::CursorLeft => {
            tui.textarea.move_left();
            edited
        }
        Action::CursorRight => {
            tui.textarea.move_right();
            edited
        }
        Action::CursorWordLeft => {
            tui.textarea.move_word_left();
            edited
        }
        Action::CursorWordRight => {
            tui.textarea.move_word_right();
            edited
        }
        Action::CursorLineStart => {
            let p = line_start(tui.textarea.text(), tui.textarea.cursor());
            tui.textarea.set_cursor(p);
            edited
        }
        Action::CursorLineEnd => {
            let p = line_end(tui.textarea.text(), tui.textarea.cursor());
            tui.textarea.set_cursor(p);
            edited
        }
        Action::DeleteCharBackward => edit(tui, |t| t.textarea.delete_backward()),
        Action::DeleteCharForward => edit(tui, |t| t.textarea.delete_forward()),
        Action::DeleteWordBackward => edit(tui, |t| t.textarea.delete_word_backward()),
        Action::DeleteWordForward => edit(tui, |t| t.textarea.delete_word_forward()),
        Action::DeleteToLineStart => edit(tui, |t| {
            let cur = t.textarea.cursor();
            let start = line_start(t.textarea.text(), cur);
            // At a line start, join with the previous line (readline).
            let start = if start == cur && cur > 0 {
                cur - 1
            } else {
                start
            };
            t.textarea.replace_range(start..cur, "");
        }),
        Action::DeleteToLineEnd => edit(tui, |t| {
            let cur = t.textarea.cursor();
            let end = line_end(t.textarea.text(), cur);
            let end = if end == cur && cur < t.textarea.text().len() {
                cur + 1
            } else {
                end
            };
            t.textarea.replace_range(cur..end, "");
        }),
    }
}

fn edit(tui: &mut Tui, f: impl FnOnce(&mut Tui)) -> Option<KeyOutcome> {
    f(tui);
    super::sync_attachments(tui);
    tui.sel = 0;
    Some(KeyOutcome::Edited)
}

fn fill_completion(tui: &mut Tui, name: &str) {
    let fill = crate::repl::completion_fill(name);
    tui.textarea.set_text(&fill);
    tui.textarea.move_to_end();
}

fn history_prev(tui: &mut Tui) {
    if tui.history.is_empty() {
        return;
    }
    if tui.history_idx.is_none() {
        tui.draft = tui.textarea.text().to_string();
        tui.history_idx = Some(tui.history.len());
    }
    if let Some(idx) = tui.history_idx
        && idx > 0
    {
        tui.history_idx = Some(idx - 1);
        let h = tui.history[idx - 1].clone();
        tui.textarea.set_text(&h);
        tui.textarea.move_to_end();
    }
}

fn history_next(tui: &mut Tui) {
    let Some(idx) = tui.history_idx else {
        return;
    };
    if idx + 1 >= tui.history.len() {
        let draft = std::mem::take(&mut tui.draft);
        tui.textarea.set_text(&draft);
        tui.history_idx = None;
    } else {
        tui.history_idx = Some(idx + 1);
        let h = tui.history[idx + 1].clone();
        tui.textarea.set_text(&h);
    }
    tui.textarea.move_to_end();
}

/// Byte offset of the start of the line holding `cur`.
pub(crate) fn line_start(text: &str, cur: usize) -> usize {
    let cur = cur.min(text.len());
    text[..cur].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte offset of the end of the line holding `cur` (before its `\n`).
pub(crate) fn line_end(text: &str, cur: usize) -> usize {
    let cur = cur.min(text.len());
    text[cur..]
        .find('\n')
        .map(|i| i + cur)
        .unwrap_or(text.len())
}

#[path = "keys_tests.rs"]
#[cfg(test)]
mod tests;
