//! game_control: install, uninstall, launch, verify, move, or open a store page via steam:// URLs.

use anyhow::Result;

use crate::steam::manifests::read_all_manifests;
use crate::steam::paths::{is_steam_running, open_steam_url};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, boxed};
use crate::util::js_num;

pub fn tool() -> Tool {
    Tool {
        name: "game_control",
        description: "Install, uninstall, launch, verify, move, or open store page",
        params: vec![
            Param::new("action", ParamType::Enum(&["install", "uninstall", "launch", "verify", "move", "store_page"]))
                .required(),
            Param::new("appid", ParamType::Number).required(),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[
            Write::Action("install"),
            Write::Action("uninstall"),
            Write::Action("launch"),
            Write::Action("verify"),
            Write::Action("move"),
        ],
        destructive: true,
        open_world: false,
    }
}

async fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_default();
    let appid = args.f64("appid").unwrap_or(0.0);
    ToolResult::or_error(run(&action, appid), &format!("Error in game_control ({action})"))
}

/// The installed game's name, or None when it isn't installed. Errors reading
/// the manifests propagate; callers that tolerate them decide what to do.
fn installed_name(appid: f64) -> Result<Option<String>> {
    Ok(read_all_manifests()?.into_iter().find(|m| m.appid as f64 == appid).map(|m| m.name))
}

/// Open `url`. A failed spawn in Node surfaces asynchronously, after the tool
/// has already answered, so the result text never reflects it.
fn open(url: &str) {
    let _ = open_steam_url(url);
}

fn not_installed(id: &str, what: &str) -> ToolResult {
    ToolResult::error(format!("Game with appid {id} is not installed. {what}"))
}

fn run(action: &str, appid: f64) -> Result<ToolResult> {
    if !is_steam_running() {
        return Ok(ToolResult::error("Steam is not running. Please start Steam first."));
    }
    let id = js_num(appid);

    let text = match action {
        "install" => {
            if let Ok(Some(name)) = installed_name(appid) {
                return Ok(ToolResult::text(format!("\"{name}\" (appid: {id}) is already installed.")));
            }
            let url = format!("steam://install/{id}");
            open(&url);
            [
                format!("Install requested for appid {id}"),
                format!("Protocol URL: {url}"),
                String::new(),
                "Steam will show the install dialog. You may need to confirm in the Steam client.".into(),
            ]
        }
        "uninstall" => {
            let game_name = match installed_name(appid) {
                Ok(Some(name)) => name,
                Ok(None) => return Ok(not_installed(&id, "Nothing to uninstall.")),
                Err(_) => format!("appid {id}"),
            };
            let url = format!("steam://uninstall/{id}");
            open(&url);
            [
                format!("Uninstall requested for \"{game_name}\" (appid: {id})"),
                format!("Protocol URL: {url}"),
                String::new(),
                "Steam will show the uninstall confirmation dialog.".into(),
            ]
        }
        "launch" => {
            let game_name = match installed_name(appid) {
                Ok(Some(name)) => name,
                Ok(None) => return Ok(not_installed(&id, "Cannot launch.")),
                Err(_) => format!("appid {id}"),
            };
            let url = format!("steam://rungameid/{id}");
            open(&url);
            [
                format!("Launching \"{game_name}\" (appid: {id})"),
                format!("Protocol URL: {url}"),
                String::new(),
                "The game launch has been requested. Steam will handle the actual launch process.".into(),
            ]
        }
        "verify" => {
            let game_name = match installed_name(appid) {
                Ok(Some(name)) => name,
                Ok(None) => return Ok(not_installed(&id, "Cannot verify.")),
                Err(_) => format!("appid {id}"),
            };
            let url = format!("steam://validate/{id}");
            open(&url);
            [
                format!("File verification requested for \"{game_name}\" (appid: {id})"),
                format!("Protocol URL: {url}"),
                String::new(),
                "Steam will verify the integrity of game files. This may take a while depending on the game size."
                    .into(),
            ]
        }
        "move" => {
            let Some(game_name) = installed_name(appid)? else {
                return Ok(not_installed(&id, "Cannot move."));
            };
            let url = format!("steam://move/{id}");
            open(&url);
            [
                format!("Move requested for \"{game_name}\" (appid: {id})"),
                format!("Protocol URL: {url}"),
                String::new(),
                "Steam will show the move dialog. Select the target library folder in the Steam client.".into(),
            ]
        }
        _ => {
            let url = format!("steam://store/{id}");
            open(&url);
            [
                format!("Opening store page for appid {id}"),
                format!("Protocol URL: {url}"),
                String::new(),
                "Steam will open the store page for this game.".into(),
            ]
        }
    };
    Ok(ToolResult::text(text.join("\n")))
}
