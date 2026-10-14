pub const SNIFF_SAMPLE_BYTES: usize = 8 * 1024;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes)
}

pub fn sniff(data: &[u8], display: &str) -> Result<(), String> {
    let sample_len = data.len().min(SNIFF_SAMPLE_BYTES);
    if data[..sample_len].contains(&0) {
        return Err(super::notices::nul_note(display));
    }
    Ok(())
}

#[path = "hygiene_tests.rs"]
#[cfg(test)]
mod tests;
