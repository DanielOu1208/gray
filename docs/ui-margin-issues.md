# Gray UI Margin Audit & Remediation Guide

This document catalogs recent UI additions in **Gray** that exhibit excessive/unnecessary vertical margins (such as the `/antigravity` model connection and `Thinking effort clamped` cards), explains the underlying architectural causes, and provides concrete fix examples for every affected UI component.

---

## 1. Root Cause Analysis

The visual bloat observed in the transcript is caused by three distinct formatting layers stacking on top of each other:

```
┌─────────────────────────────────────────────────────────────┐
│  Outer Gap: ensure_gap() -> Blank unpainted row             │
├─────────────────────────────────────────────────────────────┤
│  Card Top Margin: margin_row() (painted surface_bg)         │
│  Card Header: ⬢ Verb / Title                                │
│  Breathing Room: Blank painted row (Line::from(""))         │
│  Card Body: Indented body/detail row(s)                     │
│  Card Bottom Margin: margin_row() (painted surface_bg)      │
├─────────────────────────────────────────────────────────────┤
│  Outer Gap: ensure_gap() -> Blank unpainted row             │
└─────────────────────────────────────────────────────────────┘
```

For a simple 2-line notification like:
```text
⬢ Thinking effort clamped
  from high to off (not supported by this model)
```
the card pipeline consumes **5 painted rows** (3 of which are completely empty) plus **2 outer gap rows**, resulting in **7 terminal lines** for a short system message.

### Primary Drivers
1. **Notice Cards using Tool-Box Layout:** In commits `c854b351` and `d52b8003`, lightweight notices (`push_notice`) were routed through `push_tool_box()`, which was originally designed for large command outputs (e.g., `Ran cargo test` with 50+ lines).
2. **Unconditional Interior Blank Row:** [`format_tool_box_lines`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/cards.rs#L44-L46) always inserts an empty painted row between the header and body (`if !body.is_empty() { box_lines.push(Line::from("").style(bg_style)); }`), regardless of body length.
3. **Painted Top and Bottom Margins:** [`format_tool_box_lines`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/cards.rs#L30) and [`format_user_prompt_lines`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/rows.rs#L444) push `margin_row()` unconditionally above and below content.
4. **Redundant Gap Calls:** Callers often call `ensure_gap()` directly after `push_tool_box()`, which already called `ensure_gap()` internally.

---

## 2. Inventory of Affected UIs and How to Fix Them

### UI 1: Thinking Effort Clamp (`repl/mod.rs` & `repl/handlers.rs`)

* **Current Appearance:**
  ```text
  ✔ Connected to Antigravity subscription gemini-3.8-flash-high

  ⬢ Thinking effort clamped

    from high to off (not supported by this model)
  ```
* **Issue:** Pushed through `super::cron::push_notice(&mut t, "Thinking effort clamped", ...)`, treating a simple advisory note as a heavy tool card right below a clean 1-line action receipt.
* **Fix:** Use [`Tui::push_warning`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/boxes.rs#L278) or inline it with the connection action:
  ```rust
  // crates/gray/src/repl/mod.rs:348-356
  t.push_action(&format!("Connected to {prov_name}"), detail.as_deref());
  if let Some((old, new)) = clamped {
      t.push_warning(&format!(
          "Thinking effort clamped from {old} to {new} (not supported by this model)"
      ));
  }
  ```
* **Target Look:**
  ```text
  ✔ Connected to Antigravity subscription gemini-3.8-flash-high
  ⚠ Thinking effort clamped from high to off (not supported by this model)
  ```

---

### UI 2: General Notice Cards (`repl/cron.rs::push_notice`)

* **Locations:**
  - `Session renamed`, `Session closed`, `Session cleared`, `Project trusted` (`crates/gray/src/repl/session.rs`)
  - `Context update failed`, `Compaction failed`, `Nothing to compact` (`crates/gray/src/repl/status.rs`)
  - `Resume failed`, `Resume picker failed` (`crates/gray/src/repl/session.rs`)
  - `Plugin reloaded`, `API key reloaded` (`crates/gray/src/repl/plugin_cmds.rs`, `crates/gray/src/repl/prompt_turn.rs`)
  - `Reconnecting` stream error (`crates/gray/src/repl/session.rs`)
* **Issue:** All call `push_notice(t, verb, detail, failed)`, creating 5-line tool boxes.
* **Fix:**
  For errors/failures, use `push_error` or `push_warning`. For positive confirmations (`Session renamed`), use `push_action`:
  ```rust
  // Instead of push_notice(&mut t, "Session renamed", &name, false);
  t.push_action("Session renamed to", Some(&name));
  ```
  If kept in `push_notice`, render as a single-line or tight block rather than calling `push_tool_box`.

---

### UI 3: Tool Execution & Notice Card Formatter (`composer/transcript/cards.rs`)

* **Location:** [`format_tool_box_lines`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/cards.rs#L29-L87)
* **Issue:** Line 44 unconditionally inserts a blank row between header and body:
  ```rust
  if !body.is_empty() {
      box_lines.push(Line::from("").style(bg_style));
  }
  ```
  And lines 30 & 86 add `margin_row()` regardless of body height.
* **Fix:** Only add internal breathing room if the body has multiple lines (real command output):
  ```rust
  // crates/gray/src/composer/transcript/cards.rs
  // Only add interior breathing room for substantive outputs
  if body.len() > 1 {
      box_lines.push(Line::from("").style(bg_style));
  }
  ```

---

### UI 4: Background Job Wake Cards (`repl/cron.rs::job_card_lines`)

* **Location:** [`crates/gray/src/repl/cron.rs#L261-L288`](file:///home/vstaln/gray/crates/gray/src/repl/cron.rs#L261-L288)
* **Issue:** Generates:
  - Header: `⬢ Background job bwrap-5 · exit 0 · 4m26s`
  - Empty breathing row
  - Body: `  log ~/.gray/runs/bwrap-5.log`
  - Top & bottom painted margins
* **Fix:** Collapse the log path into the header detail or format as a compact 2-line box with no interior blank row:
  ```rust
  // Place log path cleanly without empty spacing
  pub(super) fn job_card_lines(card: &JobCard) -> (Line<'static>, Vec<Line<'static>>) {
      // ... header construction ...
      let body = vec![Line::from(vec![
          Span::styled("  log ", dim),
          Span::styled(log, Style::default().fg(th.tool_path)),
      ])];
      (Line::from(spans), body)
  }
  ```
  (Combined with UI 3 fix above, this eliminates the blank row between header and log).

---

### UI 5: Compaction Card (`repl/status.rs::compaction_card`)

* **Location:** [`crates/gray/src/repl/status.rs#L759-L770`](file:///home/vstaln/gray/crates/gray/src/repl/status.rs#L759-L770)
* **Issue:**
  ```text
  [surface_bg]
  ⬢ Context compacted · 1.2s
                                                <-- empty row
    20 messages → summary
  [surface_bg]
                                                <-- gap
  <Compaction summary markdown>
  ```
* **Fix:** Convert to an action receipt using `push_action`:
  ```rust
  // crates/gray/src/repl/status.rs:900-906
  tui.push_action(
      "Context compacted",
      Some(&format!("{elapsed_str} · {msg_count} messages \u{2192} summary")),
  );
  tui.push_compaction_summary(&summary);
  ```

---

### UI 6: User Prompt Transcript Card (`composer/transcript/rows.rs`)

* **Location:** [`crates/gray/src/composer/transcript/rows.rs#L444` & `L509`](file:///home/vstaln/gray/crates/gray/src/composer/transcript/rows.rs#L444)
* **Issue:** Every submitted command/prompt (`❯ /antigravity`) gets top and bottom painted `margin_row()` lines:
  ```rust
  lines.push(margin_row()); // Top
  // ... prompt ...
  lines.push(margin_row()); // Bottom
  ```
* **Fix:** Only apply top and bottom padding when the prompt is multi-line or has attachments:
  ```rust
  // crates/gray/src/composer/transcript/rows.rs
  let is_multiline = lines_raw.len() > 1 || !attached.is_empty();
  if is_multiline {
      lines.push(margin_row());
  }
  // ... prompt lines ...
  if is_multiline {
      lines.push(margin_row());
  }
  ```

---

### UI 7: Active Input Box Dock (`composer/draw/widgets.rs`)

* **Location:** [`crates/gray/src/composer/draw/widgets.rs#L72` & `L196`](file:///home/vstaln/gray/crates/gray/src/composer/draw/widgets.rs#L72)
* **Issue:** `build_input_box` always reserves top padding row and bottom padding row inside the active composer dock:
  ```rust
  box_lines.push(Line::from("")); // Top padding
  // ... prompt arrow ...
  box_lines.push(Line::from("")); // Bottom padding
  ```
* **Fix:** For idle or single-line prompts, omit the top/bottom blank lines or make the composer compact by default, expanding to 3+ rows only when multi-line text is entered.
