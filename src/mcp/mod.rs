//! The local stdio MCP server. Protocol traffic uses stdout; diagnostics use stderr. Neither
//! ever holds command output or source text.

pub mod ask;
pub mod locations;
pub mod search;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, schemars};
use serde::Serialize;

use locations::Roots;

/// The longest wait for the client's answer to `roots/list`.
const ROOTS_WAIT: Duration = Duration::from_secs(10);

pub struct Config {
    /// The Jev endpoint. Production passes `jev::ENDPOINT`; tests pass a fake server.
    pub endpoint: String,
    /// `TYPESAFE_API_KEY`, when it is set and not empty.
    pub key: Option<String>,
    /// `<state>/agentgrasp`.
    pub state_root: PathBuf,
}

/// The error codes of the tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    InvalidInput,
    SourceUnreadable,
    SourceChanged,
    UnsupportedEncoding,
    InputTooLarge,
    ProviderUnavailable,
    InvalidProviderResponse,
    BudgetExhausted,
    Cancelled,
}

impl Code {
    pub fn from_jev(failure: &crate::jev::Failure) -> Code {
        use crate::jev::Failure;
        match failure {
            Failure::TooLarge => Code::InputTooLarge,
            Failure::InvalidResponse => Code::InvalidProviderResponse,
            Failure::Cancelled => Code::Cancelled,
            Failure::BudgetExhausted => Code::BudgetExhausted,
            Failure::Unauthorized
            | Failure::Overloaded
            | Failure::Unavailable { .. }
            | Failure::TimedOut => Code::ProviderUnavailable,
        }
    }
}

/// A failure. The message never holds command output, source text, credentials or a provider
/// response body.
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct ToolError {
    pub code: Code,
    pub message: String,
}

impl ToolError {
    pub fn new(code: Code, message: impl Into<String>) -> ToolError {
        ToolError {
            code,
            message: message.into(),
        }
    }
}

/// The tool result: the output object as structured content. Only `invalid_input` sets
/// `isError`; for every other failure the client reads `error`.
fn result<T: Serialize>(output: &T, error: Option<&ToolError>) -> CallToolResponse {
    let value = serde_json::to_value(output).expect("tool output serializes");
    let mut result = CallToolResult::structured(value);
    result.is_error = error
        .is_some_and(|e| e.code == Code::InvalidInput)
        .then_some(true);
    result.into()
}

/// Asks the client for its roots now, so a root change reaches the next call. A client that
/// has no roots gives no allowed location.
// Roots is deprecated in protocol 2026-07-28, but Claude Code still answers roots/list, and
// it is the only way the server learns the project directories.
#[allow(deprecated)]
async fn roots(peer: &Peer<RoleServer>) -> Roots {
    let listed = tokio::time::timeout(ROOTS_WAIT, peer.list_roots()).await;
    let paths = match listed {
        Ok(Ok(listed)) => locations::root_paths(listed.roots.iter().map(|r| r.uri.as_str())),
        Ok(Err(error)) => {
            eprintln!("agentgrasp mcp: roots/list failed: {error}");
            Vec::new()
        }
        Err(_) => {
            eprintln!("agentgrasp mcp: roots/list got no answer");
            Vec::new()
        }
    };
    Roots::new(paths)
}

#[derive(Clone)]
pub struct Server {
    config: Arc<Config>,
}

impl Server {
    pub fn new(config: Config) -> Server {
        Server {
            config: Arc::new(config),
        }
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "ask answers yes/no questions about local files, such as captured command \
                 output, as probabilities. search finds the files and regions relevant to a \
                 question, as locations and scores. Neither returns file content; read the \
                 files for that.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            "ask" => {
                let output = ask::call(&self.config, arguments, &context).await;
                Ok(result(&output, output.error.as_ref()))
            }
            "search" => {
                let output = search::call(&self.config, arguments, &context).await;
                Ok(result(&output, output.error.as_ref()))
            }
            name => Err(McpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }
}

fn tools() -> Vec<Tool> {
    vec![
        Tool::new("ask", ask::DESCRIPTION, Arc::new(Default::default()))
            .with_input_schema::<ask::Input>()
            .with_output_schema::<ask::Output>(),
        Tool::new("search", search::DESCRIPTION, Arc::new(Default::default()))
            .with_input_schema::<search::Input>()
            .with_output_schema::<search::Output>(),
    ]
}

/// Serves MCP on stdin and stdout until the client closes the connection.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let service = Server::new(config).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
