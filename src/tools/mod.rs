//! The MCP tools, grouped into categories selectable with STEAM_TOOLS.

use crate::tool::Tool;

mod deals;
mod diagnostics;
mod game_config;
mod game_control;
mod game_info;
mod games;
mod insights;
mod js;
mod library;
mod player;
mod proton;
mod shortcuts;
mod status;
mod storage;
mod workshop;

type Register = fn() -> Tool;

const CATEGORIES: &[(&str, &[Register])] = &[
    ("core", &[games::tool, library::tool, status::tool]),
    ("compat", &[proton::tool, diagnostics::tool]),
    ("manage", &[game_control::tool, game_config::tool, shortcuts::tool]),
    ("storage", &[storage::tool]),
    ("api", &[player::tool, workshop::tool]),
    ("market", &[deals::tool]),
    ("insights", &[insights::tool]),
    ("content", &[game_info::tool]),
];

/// The tools of the categories named in `filter` (comma-separated), or all of them.
pub fn enabled(filter: Option<&str>) -> Vec<Tool> {
    let names: Vec<String> = match filter {
        Some(f) if !f.is_empty() => f.split(',').map(|s| s.trim().to_lowercase()).collect(),
        _ => CATEGORIES.iter().map(|(n, _)| n.to_string()).collect(),
    };
    let mut tools: Vec<Tool> = Vec::new();
    for name in names {
        if let Some((_, registers)) = CATEGORIES.iter().find(|(n, _)| *n == name) {
            for register in *registers {
                let tool = register();
                // A category named twice registers once; the SDK would have thrown here.
                if !tools.iter().any(|t| t.name == tool.name) {
                    tools.push(tool);
                }
            }
        }
    }
    tools
}
