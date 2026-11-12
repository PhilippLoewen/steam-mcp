//! diagnostics: tail the Proton compat or shader logs.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::steam::paths::get_steam_dir;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{io_error, js_num};

pub fn tool() -> Tool {
    Tool {
        name: "diagnostics",
        description: "Parse Proton compat or shader logs",
        params: vec![
            Param::new("action", ParamType::Enum(&["compat_log", "shader_log"])).required(),
            Param::new("appid", ParamType::Number),
            Param::new("lines", ParamType::Number),
        ],
        handler: |args| boxed(async move { ToolResult::or_error(run(&args), "Error parsing log") }),
        writes: &[],
        destructive: false,
        open_world: false,
    }
}

/// The last `count` lines of `path` (JS `split('\n').slice(-count)`), or a
/// "File not found" note when it doesn't exist.
fn read_last_lines(path: &Path, count: f64) -> Result<String> {
    let content = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("File not found: {}", path.display()));
        }
        Err(e) => return Err(io_error(&e, "read", path)),
    };
    let lines: Vec<&str> = content.split('\n').collect();
    let start = (lines.len() as f64 - count).max(0.0).trunc().min(lines.len() as f64) as usize;
    Ok(lines[start..].join("\n"))
}

fn run(args: &Args) -> Result<ToolResult> {
    let steam = get_steam_dir()?;
    let line_count = args.f64("lines").unwrap_or(100.0);
    let logs = steam.join("logs");

    let (title, candidates, hint): (&str, Vec<PathBuf>, &str) = if args.str("action").as_deref() == Some("compat_log") {
        (
            "Compatibility log",
            vec![logs.join("compat_log.txt"), logs.join("compat_log.previous.txt"), logs.join("compatibility_log.txt")],
            "The compat log is created when running games under Proton/Wine.",
        )
    } else {
        (
            "Shader log",
            vec![
                logs.join("shader_log.txt"),
                logs.join("shader_log.previous.txt"),
                logs.join("shader_compile.log"),
                logs.join("fossilize_engine.log"),
            ],
            "Shader logs are created during shader pre-compilation or when games compile shaders.",
        )
    };

    let mut found = None;
    for candidate in &candidates {
        if std::fs::File::open(candidate).is_ok() {
            found = Some((candidate, read_last_lines(candidate, line_count)?));
            break;
        }
    }

    // An empty log reads as "not found", as it always has.
    let Some((path, content)) = found.filter(|(_, c)| !c.is_empty()) else {
        let kind = if title == "Shader log" { "shader" } else { "compatibility" };
        let mut lines = vec![format!("No {kind} log file found."), "Searched locations:".to_string()];
        lines.extend(candidates.iter().map(|c| format!("  - {}", c.display())));
        lines.push(String::new());
        lines.push(hint.to_string());
        return Ok(ToolResult::text(lines.join("\n")));
    };

    Ok(ToolResult::text(
        [
            format!("{title}: {}", path.display()),
            format!("Showing last {} lines:", js_num(line_count)),
            "---".to_string(),
            content,
        ]
        .join("\n"),
    ))
}
