use serde_json::Value;

pub const LIMIT_ZERO_NOTE: &str = "[read: limit=0 shows nothing; omit limit or use limit>=1]";

pub fn render_got(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
}

pub fn coerce_integer(field: &str, value: &Value) -> Result<i64, String> {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else {
                Err(format!(
                    "invalid argument '{field}': expected a whole number, got {}",
                    render_got(value)
                ))
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Ok(i)
            } else if t.parse::<f64>().is_ok() {
                Err(format!(
                    "invalid argument '{field}': expected a whole number, got {t}"
                ))
            } else {
                Err(format!(
                    "invalid argument '{field}': expected integer, got {}",
                    render_got(value)
                ))
            }
        }
        _ => Err(format!(
            "invalid argument '{field}': expected integer, got {}",
            render_got(value)
        )),
    }
}

#[path = "args_tests.rs"]
#[cfg(test)]
mod tests;
