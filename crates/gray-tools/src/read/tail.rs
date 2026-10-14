use std::collections::VecDeque;

use serde_json::Value;

use super::args;
use super::stream::{LineStream, RawLine};

pub fn get_offset(args: &Value) -> Result<Option<i64>, String> {
    let Some(v) = args.get("offset") else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let o = args::coerce_integer("offset", v)?;
    if o == i64::MIN {
        return Err(format!(
            "invalid argument 'offset': expected a whole number, got {o}"
        ));
    }
    Ok(Some(o))
}

pub async fn drain_tail(s: &mut LineStream, n: u64) -> std::io::Result<VecDeque<RawLine>> {
    let mut buf = VecDeque::new();
    loop {
        match s.next_line().await? {
            None => break,
            Some(line) => {
                buf.push_back(line);
                while buf.len() as u64 > n {
                    buf.pop_front();
                }
            }
        }
    }
    Ok(buf)
}

#[path = "tail_tests.rs"]
#[cfg(test)]
mod tests;
