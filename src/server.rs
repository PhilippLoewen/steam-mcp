//! The MCP handler: lists the registered tools and dispatches calls to them.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

use crate::tool::{Tool, ToolResult};

pub const READ_ONLY_ENV: &str = "STEAM_MCP_READONLY";

/// Whether a `STEAM_MCP_READONLY` value turns read-only mode on: any
/// non-empty value but `0`, `false` or `no` does, so a typo (or a value that
/// isn't valid text) errs on the safe side.
pub fn read_only_mode(value: Option<&std::ffi::OsStr>) -> bool {
    match value {
        None => false,
        Some(v) if v.is_empty() => false,
        Some(v) => !v.to_str().is_some_and(|v| ["0", "false", "no"].iter().any(|off| v.eq_ignore_ascii_case(off))),
    }
}

#[derive(Clone)]
pub struct SteamServer {
    tools: Arc<Vec<Tool>>,
    read_only: bool,
}

impl SteamServer {
    pub fn new(tools: Vec<Tool>) -> Self {
        let read_only = read_only_mode(std::env::var_os(READ_ONLY_ENV).as_deref());
        Self { tools: Arc::new(tools), read_only }
    }

    fn annotations(&self, tool: &Tool) -> ToolAnnotations {
        let writes = !tool.writes.is_empty() && !self.read_only;
        let annotations = ToolAnnotations::new().read_only(!writes).open_world(tool.open_world);
        if writes { annotations.destructive(tool.destructive) } else { annotations }
    }

    async fn call(&self, request: CallToolRequestParams) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|t| t.name == request.name) else {
            return ToolResult::error(format!("MCP error -32602: Tool {} not found", request.name));
        };
        let args = match tool.validate(request.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolResult::error(format!(
                    "MCP error -32602: Input validation error: Invalid arguments for tool {}: {e}",
                    tool.name
                ));
            }
        };
        if self.read_only && tool.is_write(&args) {
            return ToolResult::error(format!(
                "Read-only mode is on ({READ_ONLY_ENV}), so {} can't change files, change Steam or start programs. \
                 Nothing was done.",
                tool.name
            ));
        }
        (tool.handler)(args).await
    }
}

impl ServerHandler for SteamServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("steam-mcp", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .tools
            .iter()
            .map(|t| {
                rmcp::model::Tool::new(t.name, t.description, t.input_schema(self.read_only))
                    .annotate(self.annotations(t))
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Unknown tools and invalid arguments are reported as tool errors, not
        // protocol errors, so the model sees them (as the TypeScript SDK did).
        let result = self.call(request).await;
        let content = vec![ContentBlock::text(result.text)];
        Ok(if result.is_error { CallToolResult::error(content) } else { CallToolResult::success(content) }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_errs_on_the_safe_side() {
        use std::ffi::OsStr;
        for on in ["1", "true", "yes", "TRUE", "on", "ture", " ", " 0"] {
            assert!(read_only_mode(Some(OsStr::new(on))), "{on:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(read_only_mode(Some(OsStr::from_bytes(b"\xff"))));
        }
        for off in [None, Some(""), Some("0"), Some("false"), Some("No")] {
            assert!(!read_only_mode(off.map(OsStr::new)), "{off:?}");
        }
    }

    #[test]
    fn read_only_hides_and_refuses_write_actions() {
        let tools = crate::tools::enabled(None);
        let server = SteamServer { tools: Arc::new(tools), read_only: true };
        for tool in server.tools.iter() {
            let schema = serde_json::Value::Object(tool.input_schema(true));
            let annotations = server.annotations(tool);
            assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
            assert_eq!(annotations.destructive_hint, None, "{}", tool.name);
            for write in tool.writes {
                let (param, value) = match *write {
                    crate::tool::Write::Action(v) => ("action", Some(v)),
                    crate::tool::Write::Value(p, v) => (p, Some(v)),
                    crate::tool::Write::Param(p) => (p, None),
                };
                let shown = &schema["properties"][param];
                match value {
                    Some(v) => assert!(!shown["enum"].as_array().unwrap().iter().any(|e| e == v), "{} {v}", tool.name),
                    None => assert!(shown.is_null(), "{} {param}", tool.name),
                }
            }
        }

        let call = |name: &str, args: serde_json::Value| {
            let request =
                CallToolRequestParams::new(name.to_string()).with_arguments(args.as_object().unwrap().clone());
            futures::executor::block_on(server.call(request))
        };
        for (name, args) in [
            ("game_config", serde_json::json!({"action": "set_launch_opts", "appid": 10, "options": "-x"})),
            ("game_config", serde_json::json!({"action": "presets", "appid": 10, "preset": "gamemode"})),
            ("game_config", serde_json::json!({"action": "mangohud", "mangohud_action": "reset"})),
            ("shortcuts", serde_json::json!({"action": "add", "name": "x", "exe": "/x"})),
            ("game_control", serde_json::json!({"action": "launch", "appid": 10})),
            ("library", serde_json::json!({"action": "category_delete", "name": "x"})),
            ("storage", serde_json::json!({"action": "backup"})),
            ("insights", serde_json::json!({"action": "export"})),
        ] {
            let result = call(name, args);
            assert!(result.is_error && result.text.contains("Read-only mode"), "{name}: {}", result.text);
        }
    }

    #[test]
    fn steamids_must_be_17_digits() {
        let server = SteamServer { tools: Arc::new(crate::tools::enabled(None)), read_only: false };
        for (tool, action) in [("player", "summary"), ("insights", "export"), ("deals", "wishlist")] {
            for bad in ["../../x", "7656119796026572?x=1", "7656119796026572", "765611979602657280", ""] {
                let args = serde_json::json!({ "action": action, "steamid": bad });
                let request =
                    CallToolRequestParams::new(tool.to_string()).with_arguments(args.as_object().unwrap().clone());
                let result = futures::executor::block_on(server.call(request));
                assert!(result.is_error && result.text.contains("SteamID64"), "{tool} {bad:?}: {}", result.text);
            }
        }
    }

    #[test]
    fn write_tools_are_annotated() {
        let server = SteamServer { tools: Arc::new(crate::tools::enabled(None)), read_only: false };
        let hints = |name: &str| {
            let tool = server.tools.iter().find(|t| t.name == name).unwrap();
            let a = server.annotations(tool);
            (a.read_only_hint, a.destructive_hint)
        };
        for name in ["game_control", "game_config", "shortcuts", "library"] {
            assert_eq!(hints(name), (Some(false), Some(true)), "{name}");
        }
        for name in ["storage", "insights"] {
            assert_eq!(hints(name), (Some(false), Some(false)), "{name}");
        }
        for name in ["games", "steam_status", "proton", "diagnostics", "player", "workshop", "deals", "game_info"] {
            assert_eq!(hints(name), (Some(true), None), "{name}");
        }
    }
}
