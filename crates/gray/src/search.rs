use std::path::PathBuf;

use gray_core::agent::ToolContext;
use gray_tools::search_cmd::{self, SearchArgs};

pub async fn run_find(
    pattern: &str,
    path: Option<&str>,
    limit: Option<usize>,
) -> anyhow::Result<()> {
    let ctx = ToolContext::default();
    let args = SearchArgs {
        pattern: pattern.to_string(),
        path: resolve(path, &ctx.cwd),
        limit,
        ..Default::default()
    };
    println!("{}", search_cmd::find(&args, &ctx).await);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_grep(
    pattern: &str,
    path: Option<&str>,
    limit: Option<usize>,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: Option<usize>,
) -> anyhow::Result<()> {
    let ctx = ToolContext::default();
    let args = SearchArgs {
        pattern: pattern.to_string(),
        path: resolve(path, &ctx.cwd),
        limit,
        glob: glob.map(str::to_string),
        ignore_case,
        literal,
        context,
    };
    println!("{}", search_cmd::grep(&args, &ctx).await);
    Ok(())
}

fn resolve(path: Option<&str>, cwd: &std::path::Path) -> Option<PathBuf> {
    path.map(|p| gray_core::tool_out::resolve_path(cwd, p))
}
