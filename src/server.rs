//! The MCP handler: lists the registered tools and dispatches calls to them.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

use crate::tool::{Tool, ToolResult};

#[derive(Clone)]
pub struct SteamServer {
    tools: Arc<Vec<Tool>>,
}

impl SteamServer {
    pub fn new(tools: Vec<Tool>) -> Self {
        Self { tools: Arc::new(tools) }
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
        let tools =
            self.tools.iter().map(|t| rmcp::model::Tool::new(t.name, t.description, t.input_schema())).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Unknown tools and invalid arguments are reported as tool errors, not
        // protocol errors, so the model sees them (as the TypeScript SDK did).
        let result = match self.tools.iter().find(|t| t.name == request.name) {
            None => ToolResult::error(format!("MCP error -32602: Tool {} not found", request.name)),
            Some(tool) => match tool.validate(request.arguments) {
                Ok(args) => (tool.handler)(args).await,
                Err(e) => ToolResult::error(format!(
                    "MCP error -32602: Input validation error: Invalid arguments for tool {}: {e}",
                    tool.name
                )),
            },
        };
        let content = vec![ContentBlock::text(result.text)];
        Ok(if result.is_error { CallToolResult::error(content) } else { CallToolResult::success(content) }.into())
    }
}
