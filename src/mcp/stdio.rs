use std::io::{self, BufRead, Write};
use crate::mcp::protocol::JsonRpcRequest;
use crate::mcp::sse::process_jsonrpc_request;

pub fn start_stdio_server() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(trimmed) {
            let resp = process_jsonrpc_request(req);
            let resp_str = serde_json::to_string(&resp).unwrap_or_default();
            writeln!(stdout, "{}", resp_str)?;
            stdout.flush()?;
        }
    }
    Ok(())
}
