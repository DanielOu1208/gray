use gray_core::error::{CoreError, ProviderDetail};
use gray_core::message::Message;

/// Truncates a string slice to at most `max_chars` unicode scalar values / chars.
pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Formats a token count with comma separators (e.g., 1000 -> 1,000).
pub fn fmt_usage(total: usize) -> String {
    let s = total.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (count, ch) in s.chars().rev().enumerate() {
        if count != 0 && count % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Codex steal (agent-loop.ts): pull Status/Code/Type/Message out of a
/// `status 503: {"error":{"message":..,"type":..,"code":..}}` blob so the UI
/// never dumps raw `{"model":..,"param":null}` JSON. Returns a short human
/// line; falls back to the raw detail when no JSON is found.
pub fn clean_provider_detail(detail: &str) -> String {
    let start = match detail.find('{') {
        Some(i) => i,
        None => return detail.to_string(),
    };
    let mut values =
        serde_json::Deserializer::from_str(&detail[start..]).into_iter::<serde_json::Value>();
    let parsed = match values.next() {
        Some(Ok(v)) => v,
        _ => return detail.to_string(),
    };
    let end = start + values.byte_offset();
    // Shape is usually {"model":..,"error":{"message","type","code"}} or just {"error":..}.
    let err_obj = parsed.get("error").unwrap_or(&parsed);
    let message = err_obj
        .get("message")
        .and_then(|v| v.as_str())
        .or_else(|| err_obj.as_str())
        .or_else(|| err_obj.get("code").and_then(|v| v.as_str()))
        .unwrap_or("Provider returned an error without a readable message.")
        .trim();
    let typ = err_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let code = err_obj
        .get("code")
        .and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let prefix = detail[..start]
        .trim()
        .trim_end_matches(':')
        .trim_end()
        .to_string();
    // Preserve trailing diagnostics Codex keeps (cf-ray / request-id).
    let mut suffix = String::new();
    for key in ["cf-ray: ", "request-id: ", "request_id: "] {
        if let Some(pos) = detail[end..].find(key) {
            let tail = detail[end + pos + key.len()..].trim();
            let val: String = tail
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ',')
                .collect();
            if !val.is_empty() {
                let label = if key.starts_with("cf") {
                    "cf-ray"
                } else {
                    "request-id"
                };
                if !suffix.is_empty() {
                    suffix.push_str(", ");
                }
                suffix.push_str(&format!("{label}: {val}"));
            }
        }
    }
    let mut out = if prefix.is_empty() {
        message.to_string()
    } else {
        format!("{prefix}: {message}")
    };
    if !typ.is_empty() || !code.is_empty() {
        let mut meta = vec![];
        if !typ.is_empty() {
            meta.push(format!("type: {typ}"));
        }
        if !code.is_empty() {
            meta.push(format!("code: {code}"));
        }
        out.push_str(&format!(" ({})", meta.join(", ")));
    }
    if !suffix.is_empty() {
        out.push_str(&format!(", {suffix}"));
    }
    out
}

fn display_provider_detail(detail: &str) -> String {
    let mut cleaned = detail.to_string();
    for _ in 0..4 {
        let next = clean_provider_detail(&cleaned);
        if next == cleaned {
            break;
        }
        cleaned = next;
    }
    let end = [
        " (trace ID:",
        " (trace_id:",
        " WARN ",
        " ERROR ",
        " Transient inference error",
        " (request was valid)",
    ]
    .iter()
    .filter_map(|marker| cleaned.find(marker))
    .min()
    .unwrap_or(cleaned.len());
    let text = cleaned[..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let short = truncate_chars(&text, 280);
    if short.len() < text.len() {
        format!("{short}…")
    } else {
        text
    }
}

/// Rewrites a `HH:MM UTC` clock time into the user's local zone (`HH:MM TZ`).
/// Anything that doesn't match is returned untouched.
fn localize_utc_time(raw: &str) -> String {
    use chrono::Offset;
    let offset = chrono::Local::now().offset().fix();
    match localize_utc_time_with(raw, chrono::Utc::now(), offset) {
        Some(t) => {
            let secs = offset.local_minus_utc();
            let (sign, abs) = if secs < 0 { ('-', -secs) } else { ('+', secs) };
            let (h, m) = (abs / 3600, abs % 3600 / 60);
            if secs == 0 {
                format!("{t} UTC")
            } else if m == 0 {
                format!("{t} UTC{sign}{h}")
            } else {
                format!("{t} UTC{sign}{h}:{m:02}")
            }
        }
        None => raw.to_string(),
    }
}

/// Pure core of [`localize_utc_time`]: `HH:MM UTC` or `Mon D HH:MM UTC` ->
/// the local clock reading using `offset`. Returns `None` if `raw` isn't
/// either shape.
fn localize_utc_time_with(
    raw: &str,
    now: chrono::DateTime<chrono::Utc>,
    offset: chrono::FixedOffset,
) -> Option<String> {
    use chrono::{Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
    let t = raw.trim().strip_suffix("UTC")?.trim();
    let normalized = t.replace(',', " ");
    let words: Vec<&str> = normalized.split_whitespace().collect();

    let clock = |s: &str| -> Option<(u32, u32)> {
        let (h, m) = s.split_once(':')?;
        Some((h.parse().ok()?, m.parse().ok()?))
    };
    let month_of = |s: &str| -> Option<u32> {
        const M: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        let s = s.to_ascii_lowercase();
        M.iter()
            .position(|m| s.starts_with(m))
            .map(|i| i as u32 + 1)
    };

    match words.as_slice() {
        // `HH:MM` — today, or tomorrow when that clock time already passed.
        [hm] => {
            let (h, m) = clock(hm)?;
            let mut at = now.with_hour(h)?.with_minute(m)?.with_second(0)?;
            if at < now {
                at += Duration::days(1);
            }
            Some(at.with_timezone(&offset).format("%H:%M").to_string())
        }
        // `Oct 11 00:01` — month + day + clock, no year: this year, rolling
        // forward when the moment already passed (Dec 31 -> Jan 1 resets).
        [mon, day, hm] => {
            let (h, m) = clock(hm)?;
            let (month, day) = (month_of(mon)?, day.parse::<u32>().ok()?);
            let mut at = NaiveDate::from_ymd_opt(now.year(), month, day)?.and_hms_opt(h, m, 0)?;
            if at <= now.naive_utc() {
                at = NaiveDate::from_ymd_opt(now.year() + 1, month, day)?.and_hms_opt(h, m, 0)?;
            }
            Some(
                Utc.from_utc_datetime(&at)
                    .with_timezone(&offset)
                    .format("%b %d %H:%M")
                    .to_string(),
            )
        }
        _ => None,
    }
}

fn format_model_limit(detail: &str) -> Option<String> {
    let cleaned = display_provider_detail(detail);
    let lower = cleaned.to_lowercase();
    if ![
        "quota exhausted",
        "quota exceeded",
        "insufficient_quota",
        "usage limit reached",
        "usage limit exceeded",
        "usage_limit_reached",
        "you've hit your limit",
        "you have hit your limit",
        "reached free model rate limit",
        "credit balance is too low",
        "insufficient credits",
        "insufficient balance",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return None;
    }
    let reset = regex::Regex::new(
        r"(?i)(?:will reset|resets|reset) in ([^.(]+)(?:\s*\((?:at\s+)?([^)]*)\))?",
    )
    .unwrap();
    let explanation = if let Some(captures) = reset.captures(&cleaned) {
        let duration = captures[1].trim();
        match captures.get(2) {
            Some(time) => format!(
                "Resets in {duration} ({}).",
                localize_utc_time(time.as_str().trim())
            ),
            None => format!("Resets in {duration}."),
        }
    } else if let Some(reset) = regex::Regex::new(r"(?i)\bresets ([^.\n]+)")
        .unwrap()
        .captures(&cleaned)
    {
        format!("Resets {}.", truncate_chars(reset[1].trim(), 120))
    } else {
        "Your provider's usage allowance is exhausted. Wait for it to reset or check your plan."
            .into()
    };
    Some(format!(
        "✗ Model limit reached\n  {explanation}\n  Run /models to switch to another model."
    ))
}

/// Formats a [`CoreError`] for REPL display.
/// Connection/timeout failures get a friendly, actionable message with the
/// provider's `base_url`; all other errors fall back to the generic prefix.
/// Render a [`ProviderDetail`]: vendor message + code/status + correlation
/// ids — exactly the fields format.rs used to scrape out of the message.
fn provider_detail_text(d: &ProviderDetail) -> String {
    let mut out = d.vendor_message.clone().unwrap_or_default();
    if out.is_empty()
        && let Some(status) = d.status
    {
        out = format!("status {status}");
    }
    let mut meta = Vec::new();
    if let Some(code) = &d.vendor_code {
        meta.push(format!("code: {code}"));
    }
    if let Some(status) = d.status {
        meta.push(format!("status {status}"));
    }
    if !meta.is_empty() {
        out = format!("{out} ({})", meta.join(", "));
    }
    for (label, value) in [("cf-ray", &d.cf_ray), ("request-id", &d.request_id)] {
        if let Some(v) = value {
            out.push_str(&format!(", {label}: {v}"));
        }
    }
    out
}

/// A provider failure with its response fields intact: classify on the
/// variant + structured fields, never on the formatted text.
fn format_detailed_error(e: &CoreError, d: &ProviderDetail, base_url: &str) -> String {
    let short = provider_detail_text(d);
    if let Some(message) = format_model_limit(&short) {
        return message;
    }
    match e.class() {
        CoreError::Connection(_) => format!(
            "✗ Connection failed\n  Unable to reach {base_url}: {short}\n  Check your connection or run /models to configure the provider."
        ),
        CoreError::Timeout(_) => format!(
            "✗ Request timed out\n  {short}\n  Try again or run /models to check the provider settings."
        ),
        CoreError::Auth(_) => format!(
            "✗ Auth failed (not retryable):\n  {short}\n  Check API key or run /models to reconfigure provider."
        ),
        CoreError::RateLimited(_) => format!(
            "✗ Rate limited (retryable):\n  {short}\n  Try again later or switch model via /models."
        ),
        CoreError::BadRequest(_) => format!(
            "✗ Bad request (not retryable):\n  {short}\n  Check model/provider settings via /models."
        ),
        CoreError::ContextOverflow(_) => format!(
            "✗ Context exhausted:\n  {short}\n  Start /new or compact the session to continue."
        ),
        CoreError::ServerError(_) => format!(
            "✗ Provider server error (retryable):\n  {short}\n  Try again later or run /models to switch to another model."
        ),
        CoreError::Stream(_) => {
            format!("✗ Stream broken (retryable):\n  {short}\n  Retry the turn.")
        }
        _ => format!(
            "✗ Provider error:\n  {short}\n  Provider: {base_url} — try /models if this persists."
        ),
    }
}

pub fn format_core_error(e: &CoreError, base_url: &str) -> String {
    // Response fields arrived structured: use them directly. The text arms
    // below remain the fallback for foreign errors (plugin relays) that
    // never had the fields.
    if let Some(detail) = e.detail() {
        return format_detailed_error(e, detail, base_url);
    }
    if let CoreError::Provider(detail)
    | CoreError::BadRequest(detail)
    | CoreError::Auth(detail)
    | CoreError::RateLimited(detail)
    | CoreError::ServerError(detail) = e
        && let Some(message) = format_model_limit(detail)
    {
        return message;
    }
    match e {
        CoreError::Connection(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Connection failed\n  Unable to reach {base_url}: {short}\n  Check your connection or run /models to configure the provider."
            )
        }
        CoreError::Timeout(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Request timed out\n  {short}\n  Try again or run /models to check the provider settings."
            )
        }
        CoreError::Provider(detail) => {
            // Bounded detail (never raw multi-KB dumps) + explicit
            // retryability on the first line of each classified arm.
            // Codex steal: extract Status/Code/Type/Message from JSON blobs
            // instead of dumping {"model":..,"error":{...}} raw.
            let short = display_provider_detail(detail);
            let lower = short.to_lowercase();
            if lower.contains("not supported")
                || lower.contains("unsupported")
                || lower.contains("model not found")
                || lower.contains("unknown model")
                || short.contains(" 404")
                || short.contains("status 404")
            {
                format!(
                    "✗ Bad request (not retryable):\n  {short}\n  Model may not be supported on {base_url}. Run /models to pick a valid model or provider."
                )
            } else if lower.contains("auth")
                || short.contains(" 401")
                || short.contains(" 403")
                || lower.contains("unauthorized")
            {
                format!(
                    "✗ Auth failed (not retryable):\n  {short}\n  Check API key or run /models to reconfigure provider."
                )
            } else if lower.contains("rate limit")
                || lower.contains("too many requests")
                || short.contains(" 429")
            {
                format!(
                    "✗ Rate limited (retryable):\n  {short}\n  Try again later or switch model via /models."
                )
            } else if lower.contains("bad request") || short.contains(" 400") {
                format!(
                    "✗ Bad request (not retryable):\n  {short}\n  Check model/provider settings via /models."
                )
            } else if lower.contains("server error")
                || lower.contains("status 5")
                || lower.contains("500 internal server error")
                || lower.contains("502")
                || lower.contains("503")
                || lower.contains("504")
            {
                format!(
                    "✗ Provider server error (retryable):\n  {short}\n  Try again later or run /models to switch to another model."
                )
            } else {
                // Steal codex's UnexpectedResponseError display: keep status+body but add provider hint
                format!(
                    "✗ Provider error:\n  {short}\n  Provider: {base_url} — try /models if this persists."
                )
            }
        }
        CoreError::Auth(detail) => {
            // Subscription relays surface here with no HTTP status (the 403
            // came from native inside the sidecar, already classified). Name
            // the login that failed so the next command is obvious.
            let short = display_provider_detail(detail);
            format!(
                "✗ Auth failed (not retryable):\n  {short}\n  Run /models to sign in again or update your API key."
            )
        }
        CoreError::BadRequest(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Bad request (not retryable):\n  {short}\n  Check model/provider settings via /models."
            )
        }
        CoreError::RateLimited(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Rate limited (retryable):\n  {short}\n  Try again later or switch model via /models."
            )
        }
        CoreError::ContextOverflow(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Context exhausted (not retryable):\n  {short}\n  Start /new or run /compact."
            )
        }
        CoreError::ServerError(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Provider server error (retryable):\n  {short}\n  Try again later or run /models to switch to another model."
            )
        }
        CoreError::Stream(detail) => {
            let short = display_provider_detail(detail);
            format!(
                "✗ Stream broken (retryable):\n  {short}\n  Retrying the turn usually succeeds."
            )
        }
        CoreError::LoopDetected(detail) => {
            let short = truncate_chars(detail, 600);
            format!("✗ Tool loop detected (not retryable): {short}")
        }
        CoreError::Cancelled => "■ Cancelled.".to_string(),
        _ => format!("agent error: {e}"),
    }
}

/// Builds the user message with MIME-driven attachments (opencode parity):
/// images normalized (downscaled, capped); video, PDF and audio as native
/// media carrying a fallback (contact sheet, PDF text, a note) that the
/// provider sends instead on a model without that input — the same parts a
/// `cat` of the file produces. Anything else is reported loudly.
pub(crate) fn build_user_message_with_attachments(
    text: &str,
    paths: &[std::path::PathBuf],
) -> Message {
    use gray_core::message::ContentBlock;
    use gray_tools::view::Attached;
    if paths.is_empty() {
        return Message::user(text);
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(ContentBlock::text(text.to_string()));
    }
    for path in paths {
        if !gray_tools::images::is_viewable_extension(path) {
            blocks.push(ContentBlock::text(format!(
                "(attached file {} skipped: unsupported type)",
                path.display()
            )));
            continue;
        }
        blocks.push(match gray_tools::view::attach(path) {
            Ok(Attached::Image(img, _)) => ContentBlock::image(img.media_type, img.data),
            Ok(Attached::Media(m, _)) => ContentBlock::media(m.media_type, m.data, m.fallback),
            Ok(Attached::Text(t, _)) => ContentBlock::text(t),
            Err(e) => ContentBlock::text(format!("(attachment skipped: {e})")),
        });
    }
    Message::new(gray_core::message::Role::User, blocks)
}

/// ANSI dim + italic — pi's styling for rendered thinking blocks
/// (italic muted color; dim stands in for pi's `thinkingText` theme color).
pub const THINKING_STYLE: &str = "\x1b[2m\x1b[3m";

/// Formats a turn duration like the TUI `Worked for` line: `850ms`, `6s`, `6.5s`, `2m 5s`.
pub fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 1.0 {
        format!("{ms}ms")
    } else if secs < 60.0 {
        let s = format!("{secs:.1}s");
        if s.ends_with(".0s") {
            s.replacen(".0s", "s", 1)
        } else {
            s
        }
    } else {
        let total_s = ms / 1000;
        let m = total_s / 60;
        let s = total_s % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    }
}

#[path = "format_tests.rs"]
#[cfg(test)]
mod tests;
