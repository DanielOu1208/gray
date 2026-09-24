use clap::Parser;
use std::path::Path;

use crate::Cli;

#[test]
fn structured_input_cli_accepts_file_and_json_mode() {
    let cli = Cli::try_parse_from(["gray", "--input-json", "event.json", "--json"]).unwrap();
    assert_eq!(cli.input_json.as_deref(), Some(Path::new("event.json")));
    assert!(cli.print.is_none());
    assert!(cli.json);
}

#[test]
fn structured_input_cli_rejects_text_mode_and_missing_json_mode() {
    let both = Cli::try_parse_from([
        "gray",
        "--print",
        "hello",
        "--input-json",
        "event.json",
        "--json",
    ]);
    assert!(both.is_err());

    let json_only = Cli::try_parse_from(["gray", "--json"]);
    assert!(json_only.is_err());
}

#[test]
fn text_print_cli_remains_valid() {
    let cli = Cli::try_parse_from(["gray", "--print", "hello", "--json"]).unwrap();
    assert_eq!(cli.print.as_deref(), Some("hello"));
    assert!(cli.input_json.is_none());
}
