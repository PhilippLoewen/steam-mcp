//! steam-mcp: an MCP server over stdio for the local Steam library.
//!
//! STEAM_TOOLS=core,compat,... limits the registered tool categories;
//! unset, all are registered.

mod server;
mod steam;
mod tool;
mod tools;
mod util;
mod vdf;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() {
    let filter = std::env::var("STEAM_TOOLS").ok();
    let server = server::SteamServer::new(tools::enabled(filter.as_deref()));
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Fatal error: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("Steam MCP server running on stdio");
    if let Err(e) = running.waiting().await {
        eprintln!("Fatal error: {e}");
        std::process::exit(1);
    }
}
