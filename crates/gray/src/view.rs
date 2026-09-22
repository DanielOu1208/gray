//! `gray view` — show an image file as an image, not as text.
//!
//! The bash tool claims `gray view <path>` before the shell runs, exactly as
//! it claims `cat <path>`, so in an agent session the image is attached as a
//! vision block and this command's own stdout is for the human at a terminal:
//! it names what it showed. The decode, the 2000px downscale and the error
//! wording live in `gray_tools:: view` so both surfaces stay in agreement.

use std::path::Path;

/// What `gray view PATH...` reports: the lines to print for the images it
/// showed and the errors for the ones it could not. Kept as data, so the
/// exit code and the wording are testable without capturing stdout.
pub fn view_lines(paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut shown = Vec::new();
    let mut failed = Vec::new();
    for raw in paths {
        match gray_tools::view::load(Path::new(raw)) {
            Ok(shown_img) => shown.push(format!("viewed {}", shown_img.path.display())),
            Err(e) => failed.push(e.to_string()),
        }
    }
    (shown, failed)
}

/// `gray view PATH...`: one line per image, non-zero exit if any path failed.
pub fn run_cli(paths: &[String]) -> anyhow::Result<()> {
    let (shown, failed) = view_lines(paths);
    for line in shown {
        println!("{line}");
    }
    for err in &failed {
        eprintln!("gray view: {err}");
    }
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

#[path = "view_tests.rs"]
#[cfg(test)]
mod tests;
