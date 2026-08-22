pub mod handler;
pub mod profiles;
pub mod protocol;
pub mod security;
pub mod sse;
pub mod stdio;
pub mod tools;

pub use handler::{process_jsonrpc_request, process_jsonrpc_request_with_config, process_jsonrpc_request_with_profile};
pub use profiles::{resolve_profile, resolve_profile_for, McpProfile, ProfileSource, ResolvedProfile, PROFILE_ENV_VAR};
pub use protocol::{CallToolResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse, Tool};
pub use security::TOKEN_ENV_VAR;
pub use sse::{
    build_router, build_router_with, build_router_with_profile, start_http_server, start_sse_server, start_sse_server_with_options,
    HttpTransport, ServerSecurity, SseServerOptions,
};
pub use stdio::{run_stdio, run_stdio_with_profile, start_stdio_server, start_stdio_server_with};
pub use tools::{execute_tool, execute_tool_with_config, get_tools_list, tools_for_profile};
