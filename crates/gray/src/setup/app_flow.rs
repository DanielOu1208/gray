//! The app setup flow's headless core: ask for exactly what the app
//! declares, write it privately, prove it with the app's own doctor, then
//! register the app's tool. The REPL modal (task 5b) rides the same core.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use super::registry::{FieldKind, SetupDecl, SetupField};
use super::write_config::{Supplied, write_config};

/// Fields the flow must still ask about: everything non-derived the app's
/// config does not already answer. Required first, in declaration order.
pub fn plan_missing<'a>(decl: &'a SetupDecl, home: &Path) -> Vec<&'a SetupField> {
    let path = home.join(decl.config_path);
    decl.fields
        .iter()
        .filter(|f| !matches!(f.kind, FieldKind::Derived))
        .filter(|f| !super::registry::key_present(&path, f.key))
        .collect()
}

/// `--field key=value` answers, checked against the declaration. Secrets are
/// flagged so `Supplied` can redact them everywhere else.
pub fn supplied_from_flags(decl: &SetupDecl, fields: &[String]) -> Result<Supplied> {
    let mut out = Supplied::default();
    for raw in fields {
        let (key, value) = raw
            .split_once('=')
            .with_context(|| format!("--field wants key=value, got '{raw}'"))?;
        let field = decl
            .field(key)
            .with_context(|| format!("'{key}' is not a setup field of this app"))?;
        anyhow::ensure!(
            !matches!(field.kind, FieldKind::Derived),
            "'{key}' is derived; gray fills it in"
        );
        out.insert(key, value.to_string(), field.secret);
    }
    Ok(out)
}

/// Required fields the flags did not answer. Non-empty means the flow cannot
/// finish yet — the caller reports these instead of writing a half-config.
pub fn missing_required<'a>(decl: &'a SetupDecl, supplied: &Supplied) -> Vec<&'a SetupField> {
    decl.fields
        .iter()
        .filter(|f| f.is_required() && supplied.get(f.key).is_none())
        .collect()
}

/// One human line per missing field, with the portal URL when the app named
/// one. No values, no secrets.
pub fn describe_missing(missing: &[&SetupField]) -> String {
    missing
        .iter()
        .map(|f| match f.url {
            Some(url) => format!("{} (get it at {url})", f.description),
            None => f.description.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// The app's verify command, with the declaration's argv[0] (the app binary)
/// resolved through the plugin registry.
pub fn verify_argv(app: &str, home: &Path, decl: &SetupDecl) -> Result<Vec<String>> {
    let (_bin, args) = decl
        .verify
        .split_first()
        .context("the app's declaration has no verify command")?;
    let mut argv = crate::plugin_cli::command_argv(home, app)?;
    argv.extend(args.iter().map(|a| a.to_string()));
    Ok(argv)
}

/// The app's register command (`<app-bin> register`).
pub fn register_argv(app: &str, home: &Path) -> Result<Vec<String>> {
    let mut argv = crate::plugin_cli::command_argv(home, app)?;
    argv.push("register".to_string());
    Ok(argv)
}

/// Running one of the app's own commands and catching its real output.
pub struct StepOutput {
    pub ok: bool,
    pub output: String,
}

pub fn run_step(argv: &[String]) -> StepOutput {
    let Some((program, args)) = argv.split_first() else {
        return StepOutput {
            ok: false,
            output: "empty command".to_string(),
        };
    };
    match Command::new(program).args(args).output() {
        Ok(out) => StepOutput {
            ok: out.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        },
        Err(e) => StepOutput {
            ok: false,
            output: format!("could not run {program}: {e}"),
        },
    }
}

/// Headless twin of the /gateway flow: write what the flags answer, prove it
/// with the app's doctor, register the app's tool, and say so. Anything
/// missing is reported, never guessed; nothing success-shaped is printed
/// until the doctor agrees.
pub fn run_headless(app: &str, fields: &[String]) -> Result<()> {
    let home = crate::plugin_cli::home()?;
    let decl = crate::plugin_cli::setup_decl(app)
        .with_context(|| format!("gray has no setup declaration for '{app}'"))?;
    let supplied = supplied_from_flags(decl, fields)?;
    let missing = missing_required(decl, &supplied);
    if !missing.is_empty() {
        anyhow::bail!("{} still needs:\n  {}", app, describe_missing(&missing));
    }
    write_config(&home.join(decl.config_path), decl, &supplied, &home)
        .context("could not write the app's config")?;
    let verify = run_step(&verify_argv(app, &home, decl)?);
    if !verify.ok {
        anyhow::bail!(
            "{} was written but its doctor disagrees:\n{}",
            app,
            verify.output.trim()
        );
    }
    if decl.post_steps.contains(&"register") {
        let register = run_step(&register_argv(app, &home)?);
        anyhow::ensure!(
            register.ok,
            "the config works but registering the tool failed:\n{}",
            register.output.trim()
        );
    }
    println!("{app} is set up.");
    Ok(())
}

#[path = "app_flow_tests.rs"]
#[cfg(test)]
mod tests;
