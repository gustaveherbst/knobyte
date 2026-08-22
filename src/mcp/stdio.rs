//! Newline-delimited JSON-RPC over stdin/stdout.
//!
//! stdout carries protocol messages only; diagnostics go to stderr.

use std::io::{self, BufRead, Write};

use crate::config::KnobyteConfig;
use crate::mcp::handler::{handle_text, server_config, PARSE_ERROR};
use crate::mcp::protocol::JsonRpcResponse;

/// Run the stdio MCP server for the project discovered from the working directory.
pub fn start_stdio_server() -> io::Result<()> {
    let config = server_config();
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_stdio(stdin.lock(), stdout.lock(), &config)
}

/// Serve JSON-RPC messages read line by line from `reader`, writing one
/// response line per request to `writer`. Notifications produce no output;
/// malformed JSON produces a -32700 parse error with a null id.
pub fn run_stdio<R: BufRead, W: Write>(mut reader: R, mut writer: W, config: &KnobyteConfig) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        let response = match std::str::from_utf8(&buf) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                handle_text(trimmed, config)
            }
            Err(e) => {
                eprintln!("[knobyte mcp] received non-UTF-8 input: {}", e);
                Some(
                    serde_json::to_string(&JsonRpcResponse::error(
                        Some(serde_json::Value::Null),
                        PARSE_ERROR,
                        "Parse error: input is not valid UTF-8",
                    ))
                    .unwrap_or_default(),
                )
            }
        };
        if let Some(resp) = response {
            writeln!(writer, "{}", resp)?;
            writer.flush()?;
        }
    }
}
