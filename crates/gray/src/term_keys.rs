//! Keyboard-enhancement negotiation for the composer.
//!
//! Shift+Enter and Alt+Enter only reach gray as *distinct keys* when the
//! terminal is asked to report modifiers. Without it every terminal folds
//! them into a bare Enter, which submits instead of inserting a newline.
//! gray asks for Kitty's progressive enhancement (crossterm's
//! `PushKeyboardEnhancementFlags`) and pops it on drop, so the parent
//! shell never inherits enhanced key reporting.
//!
//! Ported from Codex's TUI (`codex-rs/tui/src/tui/keyboard_modes.rs`), which
//! gets this right in three parts that are all load-bearing:
//!
//! 1. **Ask for the right flags.** `DISAMBIGUATE_ESCAPE_CODES |
//!    REPORT_ALTERNATE_KEYS` — disambiguation alone leaves modified Enter
//!    indistinguishable on several terminals.
//! 2. **Leave `REPORT_EVENT_TYPES` off where it backfires.** Ghostty and
//!    iTerm leak key-release events as spurious presses, and tmux's
//!    `extended-keys-format xterm` loses Shift+Enter entirely once event
//!    types are reported.
//! 3. **Degrade, don't assume.** Terminals without the protocol ignore the
//!    sequence, `GRAY_NO_KEYBOARD_ENHANCEMENT` is an escape hatch for the
//!    ones that misbehave, and Alt+Enter (handled alongside Shift+Enter in
//!    both input paths) keeps working when the terminal cannot report
//!    modifiers at all.

use std::io::stdout;

use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

/// xterm's `modifyOtherKeys`: mode 2 makes the terminal report every key
/// with its modifier, which is the older fallback for a terminal that
/// speaks no Kitty protocol. crossterm 0.28 has no command for it (added in
/// 0.29), and these two private-mode sequences are the whole contract, so
/// they are written directly rather than bumping the terminal stack.
const MODIFY_OTHER_KEYS_ON: &str = "\x1b[>4;2m";
const MODIFY_OTHER_KEYS_OFF: &str = "\x1b[>4;0m";

/// The exact bytes crossterm writes for a flag set: `CSI > <bits> u`,
/// where the bits are the Kitty protocol's own numbering. Rendered here so
/// the negotiation can be pinned by a test instead of by a terminal.
pub fn push_sequence(flags: KeyboardEnhancementFlags) -> String {
    use crossterm::Command as _;
    // crossterm's Command renders into a fmt::Write, not an io::Write.
    let mut out = String::new();
    PushKeyboardEnhancementFlags(flags)
        .write_ansi(&mut out)
        .expect("write to String cannot fail");
    out
}

fn write_mode(seq: &str) {
    use std::io::Write as _;
    let mut out = stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Escape hatch: set to `1`/`true`/`yes` to leave the terminal's key
/// reporting exactly as gray found it.
pub const DISABLE_ENV: &str = "GRAY_NO_KEYBOARD_ENHANCEMENT";

/// True when the operator (or a misbehaving terminal) has switched the
/// whole thing off.
pub fn disabled() -> bool {
    matches!(
        std::env::var(DISABLE_ENV).ok().as_deref().map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// The flags to request, given what we know about the terminal and its
/// multiplexer. Split out from the environment so it is testable.
///
/// `REPORT_EVENT_TYPES` is deliberately withheld for Ghostty/iTerm (they
/// leak release events) and for tmux in `xterm` extended-keys format (it
/// drops Shift+Enter) — Codex's rule, with the same reasoning.
pub fn flags_for(
    term_program: Option<&str>,
    tmux_extended_keys_format: Option<&str>,
) -> KeyboardEnhancementFlags {
    let base = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    let name = term_program.unwrap_or_default().to_ascii_lowercase();
    let leaks_release_events = name.contains("ghostty") || name.contains("iterm");
    let tmux_loses_shift_enter = tmux_extended_keys_format.is_some_and(|f| f == "xterm");
    if leaks_release_events || tmux_loses_shift_enter {
        base
    } else {
        base | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
    }
}

/// tmux can only pass modified keys through when it is formatting as
/// csi-u; its `xterm` format needs the older `modifyOtherKeys` sequence
/// instead, and older tmux exposes neither.
pub fn should_enable_modify_other_keys(
    in_tmux: bool,
    tmux_extended_keys_format: Option<&str>,
) -> bool {
    in_tmux && matches!(tmux_extended_keys_format, Some("csi-u"))
}

/// Running under tmux (or screen, which needs the same care).
pub fn in_multiplexer() -> bool {
    std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some()
}

/// tmux's `extended-keys-format`, or `None` when tmux is absent or too old
/// to report it. Two probes, both non-interactive and silent: a hung tmux
/// cannot wedge startup (they inherit a null stdin/stderr and have no
/// timeout — see the note in `probe_timeout`).
pub fn tmux_extended_keys_format() -> Option<String> {
    for args in [
        ["display-message", "-p", "#{extended-keys-format}"],
        ["show-options", "-gqv", "extended-keys-format"],
    ] {
        let output = std::process::Command::new("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        if let Some(value) = String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
    }
    None
}

/// Pushes the negotiated enhancement for as long as it is held, and
/// restores the terminal exactly (including the `modifyOtherKeys` escape)
/// on drop. One guard per place that owns stdin: the prompt, and the
/// turn-time key watcher.
#[derive(Debug)]
pub struct KeyboardEnhancementGuard {
    pushed: bool,
    modify_other_keys: bool,
}

impl KeyboardEnhancementGuard {
    /// Negotiate from the live environment.
    pub fn push() -> Self {
        if disabled() {
            return Self {
                pushed: false,
                modify_other_keys: false,
            };
        }
        let in_tmux = in_multiplexer();
        let format = if in_tmux {
            tmux_extended_keys_format()
        } else {
            None
        };
        let flags = flags_for(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            format.as_deref(),
        );
        let modify_other_keys = should_enable_modify_other_keys(in_tmux, format.as_deref());
        let pushed = crossterm::execute!(stdout(), PushKeyboardEnhancementFlags(flags)).is_ok();
        if modify_other_keys {
            write_mode(MODIFY_OTHER_KEYS_ON);
        }
        Self {
            pushed,
            modify_other_keys,
        }
    }

    /// Whether the terminal is now reporting modified keys. False on a
    /// terminal that ignored the sequence, so callers can hint the Alt+Enter
    /// fallback instead of pretending Shift+Enter works.
    pub fn active(&self) -> bool {
        self.pushed
    }
}

impl Drop for KeyboardEnhancementGuard {
    fn drop(&mut self) {
        if self.modify_other_keys {
            write_mode(MODIFY_OTHER_KEYS_OFF);
        }
        if self.pushed {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disambiguation_and_alternate_keys_are_always_requested() {
        // The pair that makes modified Enter distinguishable; dropping
        // either is what makes Shift+Enter arrive as a bare Enter.
        for (term, tmux) in [
            (None, None),
            (Some("ghostty"), None),
            (Some("iTerm.app"), None),
            (Some("xterm-kitty"), Some("xterm")),
            (Some("tmux"), Some("csi-u")),
        ] {
            let f = flags_for(term, tmux);
            assert!(
                f.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
                "disambiguate on for {term:?}/{tmux:?}"
            );
            assert!(
                f.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS),
                "alternate keys on for {term:?}/{tmux:?}"
            );
        }
    }

    #[test]
    fn event_types_are_withheld_where_they_backfire() {
        // Ghostty and iTerm leak key-release events as extra presses.
        for term in ["ghostty", "Ghostty", "iTerm.app"] {
            assert!(
                !flags_for(Some(term), None).contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
                "no event types for {term}"
            );
        }
        // tmux's xterm extended-keys format loses Shift-Enter with them on.
        assert!(
            !flags_for(Some("tmux"), Some("xterm"))
                .contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        );
        // Everywhere else they are fine and keep repeat classification.
        assert!(
            flags_for(Some("xterm-256color"), None)
                .contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        );
        assert!(
            flags_for(Some("tmux"), Some("csi-u"))
                .contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        );
    }

    #[test]
    fn the_push_is_the_kitty_csi_u_sequence() {
        // DISAMBIGUATE(1) | EVENT_TYPES(2) | ALTERNATE_KEYS(4) = 7.
        let full = flags_for(Some("xterm-256color"), None);
        assert_eq!(push_sequence(full), "\x1b[>7u");
        // Ghostty/iTerm/tmux-xterm: without EVENT_TYPES it is 5.
        assert_eq!(push_sequence(flags_for(Some("ghostty"), None)), "\x1b[>5u");
        // The legacy gray behavior was the single disambiguate bit.
        assert_eq!(
            push_sequence(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            "\x1b[>1u"
        );
    }

    #[test]
    fn the_modify_other_keys_sequences_are_the_xterm_private_modes() {
        // Hand-written, so pin the exact bytes: mode 2 on, mode 0 off.
        assert_eq!(MODIFY_OTHER_KEYS_ON.as_bytes(), b"\x1b[>4;2m");
        assert_eq!(MODIFY_OTHER_KEYS_OFF.as_bytes(), b"\x1b[>4;0m");
    }

    #[test]
    fn modify_other_keys_is_tmux_with_csi_u_only() {
        assert!(should_enable_modify_other_keys(true, Some("csi-u")));
        // Not outside tmux, not for xterm formatting, not for old tmux.
        assert!(!should_enable_modify_other_keys(false, Some("csi-u")));
        assert!(!should_enable_modify_other_keys(true, Some("xterm")));
        assert!(!should_enable_modify_other_keys(true, None));
    }
}
