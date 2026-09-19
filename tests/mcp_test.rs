use serde_json::json;
use knobyte::mcp::protocol::JsonRpcRequest;
use knobyte::mcp::sse::process_jsonrpc_request;
use knobyte::mcp::tools::get_tools_list;

#[test]
fn test_mcp_tools_list() {
    let tools = get_tools_list();
    assert!(tools.iter().any(|t| t.name == "knobyte_check"));
    assert!(tools.iter().any(|t| t.name == "knobyte_log"));
    assert!(tools.iter().any(|t| t.name == "knobyte_timeline"));
    assert!(tools.iter().any(|t| t.name == "knobyte_graph_query"));
    assert!(tools.iter().any(|t| t.name == "knobyte_wiki_query"));
    // CozoDB graph & vector tools
    assert!(tools.iter().any(|t| t.name == "knobyte_vector_search"));
    assert!(tools.iter().any(|t| t.name == "knobyte_cozo_datalog"));
    assert!(tools.iter().any(|t| t.name == "knobyte_cozo_pagerank"));
    assert!(tools.iter().any(|t| t.name == "knobyte_cozo_shortest_path"));
    assert!(tools.iter().any(|t| t.name == "knobyte_sync_groundings"));
    // New tools for continuity, harvesting, and contextual memory
    assert!(tools.iter().any(|t| t.name == "knobyte_session_start"));
    assert!(tools.iter().any(|t| t.name == "knobyte_workstream_step_update"));
    assert!(tools.iter().any(|t| t.name == "knobyte_file_context"));
    assert!(tools.iter().any(|t| t.name == "knobyte_harvest"));
    assert_eq!(tools.len(), 26);
}

#[test]
fn test_mcp_jsonrpc_initialize() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "initialize".to_string(),
        params: None,
    };

    let resp = process_jsonrpc_request(req);
    assert_eq!(resp.id, Some(json!(1)));
    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert_eq!(result.get("protocolVersion").and_then(|v| v.as_str()), Some("2024-11-05"));
    assert_eq!(result.get("serverInfo").and_then(|v| v.get("name")).and_then(|v| v.as_str()), Some("knobyte"));
    // Verify instructions are provided
    let instructions = result.get("instructions").and_then(|v| v.as_str()).unwrap();
    assert!(instructions.contains("Knobyte Agent Operating Rules"));
    assert!(instructions.contains("knobyte_session_start"));
    // Verify capabilities include tools, resources, and prompts
    let caps = result.get("capabilities").unwrap();
    assert!(caps.get("tools").is_some());
    assert!(caps.get("resources").is_some());
    assert!(caps.get("prompts").is_some());
}

#[test]
fn test_mcp_jsonrpc_tools_list() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(2)),
        method: "tools/list".to_string(),
        params: None,
    };

    let resp = process_jsonrpc_request(req);
    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    let tools = result.get("tools").and_then(|v| v.as_array()).unwrap();
    assert!(!tools.is_empty());
    assert!(tools.iter().any(|t| t.get("name").and_then(|n| n.as_str()) == Some("knobyte_vector_search")));
    assert!(tools.iter().any(|t| t.get("name").and_then(|n| n.as_str()) == Some("knobyte_cozo_datalog")));
    assert!(tools.iter().any(|t| t.get("name").and_then(|n| n.as_str()) == Some("knobyte_session_start")));
}

#[test]
fn test_mcp_resources_and_prompts() {
    // Test resources/list
    let req_res = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(3)),
        method: "resources/list".to_string(),
        params: None,
    };
    let resp_res = process_jsonrpc_request(req_res);
    assert!(resp_res.error.is_none());
    let res_list = resp_res.result.unwrap().get("resources").and_then(|v| v.as_array()).cloned().unwrap();
    assert!(res_list.iter().any(|r| r.get("uri").and_then(|u| u.as_str()) == Some("knobyte://context/stack")));

    // Test prompts/list
    let req_prompts = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(4)),
        method: "prompts/list".to_string(),
        params: None,
    };
    let resp_prompts = process_jsonrpc_request(req_prompts);
    assert!(resp_prompts.error.is_none());
    let prompts_list = resp_prompts.result.unwrap().get("prompts").and_then(|v| v.as_array()).cloned().unwrap();
    assert!(prompts_list.iter().any(|p| p.get("name").and_then(|n| n.as_str()) == Some("start-session")));
    assert!(prompts_list.iter().any(|p| p.get("name").and_then(|n| n.as_str()) == Some("impact-analysis")));

    // Test prompts/get
    let req_get_prompt = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(5)),
        method: "prompts/get".to_string(),
        params: Some(json!({
            "name": "impact-analysis",
            "arguments": { "symbol": "authenticate" }
        })),
    };
    let resp_get_prompt = process_jsonrpc_request(req_get_prompt);
    assert!(resp_get_prompt.error.is_none());
    let prompt_val = resp_get_prompt.result.unwrap();
    let msg = prompt_val.get("messages").and_then(|m| m.as_array()).unwrap();
    let text = msg[0].get("content").and_then(|c| c.get("text")).and_then(|t| t.as_str()).unwrap();
    assert!(text.contains("authenticate"));
}
