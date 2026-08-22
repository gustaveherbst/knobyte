//! Security helpers shared by the MCP transports and tool handlers:
//! path confinement, identifier validation, Origin/Host checks and
//! bearer-token handling.

use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

use crate::config::KnobyteConfig;

/// Environment variable holding the bearer token required by the HTTP transport.
pub const TOKEN_ENV_VAR: &str = "KNOBYTE_MCP_TOKEN";

/// Validate an identifier that will be used to build a file name
/// (e.g. `<dir>/<id>.json`). Rejects empty ids, path separators,
/// `..`, NUL bytes and leading dots.
pub fn validate_id(id: &str) -> Result<&str, String> {
    if id.is_empty() {
        return Err("identifier must not be empty".to_string());
    }
    if id.len() > 200 {
        return Err("identifier is too long".to_string());
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') || id.contains("..") || id.starts_with('.') {
        return Err(format!("invalid identifier '{}'", id));
    }
    if id.chars().any(|c| c.is_control()) {
        return Err(format!("invalid identifier '{}'", id));
    }
    Ok(id)
}

/// Resolve `rel` inside `base`, refusing absolute paths, `..` components and
/// symlinks that resolve outside `base`. The file must exist.
pub fn resolve_confined_path(base: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if rel.contains('\0') {
        return Err("path contains a NUL byte".to_string());
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        return Err("absolute paths are not allowed".to_string());
    }
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err("path escapes scaffold root".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths are not allowed".to_string())
            }
        }
    }
    // Backslash separators are treated as traversal attempts on every platform.
    if rel.split(['/', '\\']).any(|seg| seg == "..") {
        return Err("path escapes scaffold root".to_string());
    }

    let canonical_base = base
        .canonicalize()
        .map_err(|_| "scaffold root does not exist".to_string())?;
    let joined = canonical_base.join(rel_path);
    let canonical = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // The target does not exist (or is a dangling symlink). If it is a
            // symlink, refuse it outright; otherwise report "not found".
            if joined.symlink_metadata().is_ok() {
                return Err("path escapes scaffold root".to_string());
            }
            return Err(format!("File not found: {}", rel));
        }
    };
    if !canonical.starts_with(&canonical_base) {
        return Err("path escapes scaffold root".to_string());
    }
    Ok(canonical)
}

/// Check that an optional caller-supplied `projectRoot` refers to the server's
/// own project root. Any other directory is rejected.
pub fn check_project_root_arg(arg: Option<&serde_json::Value>, config: &KnobyteConfig) -> Result<(), String> {
    let value = match arg {
        None | Some(serde_json::Value::Null) => return Ok(()),
        Some(v) => v,
    };
    let requested = value
        .as_str()
        .ok_or_else(|| "'projectRoot' must be a string".to_string())?;
    let requested_canon = Path::new(requested).canonicalize().ok();
    let server_canon = config.project_root.canonicalize().unwrap_or_else(|_| config.project_root.clone());
    match requested_canon {
        Some(p) if p == server_canon => Ok(()),
        _ => Err(format!(
            "'projectRoot' override is not permitted; this server only serves {}",
            server_canon.display()
        )),
    }
}

/// True when `host` (a bind address or a Host-header host part) is loopback.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim();
    let h = h.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(h);
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Split an authority (`host[:port]`, `[v6]:port`) into its host part.
pub fn authority_host(authority: &str) -> &str {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &a[..end + 2];
        }
        return a;
    }
    match a.rfind(':') {
        // A bare IPv6 address without brackets has several colons.
        Some(idx) if a[..idx].find(':').is_none() => &a[..idx],
        _ => a,
    }
}

/// Parse an Origin header into `(scheme, authority)`.
fn parse_origin(origin: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = origin.split_once("://")?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some((scheme, rest))
}

/// Decide whether a request with the given `Origin` and `Host` headers is allowed.
///
/// * A missing Origin (non-browser client) is allowed.
/// * `http(s)://localhost`, `127.0.0.1` and `[::1]` origins (any port) are allowed.
/// * When the server is bound to a non-loopback interface (where a bearer token
///   is always enforced), a same-origin request whose Origin authority equals the
///   Host header is also allowed.
pub fn origin_allowed(origin: Option<&str>, host_header: Option<&str>, loopback_bind: bool) -> bool {
    let origin = match origin {
        None => return true,
        Some(o) => o.trim(),
    };
    let (scheme, authority) = match parse_origin(origin) {
        Some(p) => p,
        None => return false,
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    if is_loopback_host(authority_host(authority)) {
        return true;
    }
    if !loopback_bind {
        if let Some(h) = host_header {
            return h.trim().eq_ignore_ascii_case(authority);
        }
    }
    false
}

/// For loopback binds the Host header must name a loopback host; this blocks
/// DNS-rebinding attacks. Non-loopback binds accept any Host (token auth applies).
pub fn host_allowed(host_header: Option<&str>, loopback_bind: bool) -> bool {
    if !loopback_bind {
        return true;
    }
    match host_header {
        // HTTP/1.0 clients may omit Host; they cannot be a rebinding browser.
        None => true,
        Some(h) => is_loopback_host(authority_host(h)),
    }
}

/// Constant-time byte comparison (length leak only).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a random 256-bit-ish token (two v4 UUIDs, 244 random bits) as hex.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Extract a bearer token from an `Authorization` header value.
pub fn bearer_from_header(value: &str) -> Option<&str> {
    let v = value.trim();
    let (scheme, token) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let t = token.trim();
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(validate_id("draft_123").is_ok());
        assert!(validate_id("../x").is_err());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("a\\b").is_err());
        assert!(validate_id("..").is_err());
        assert!(validate_id("").is_err());
    }

    #[test]
    fn hosts_and_origins() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert_eq!(authority_host("localhost:3001"), "localhost");
        assert_eq!(authority_host("[::1]:3001"), "[::1]");
        assert!(origin_allowed(None, None, true));
        assert!(origin_allowed(Some("http://localhost:5173"), None, true));
        assert!(origin_allowed(Some("https://127.0.0.1"), None, true));
        assert!(origin_allowed(Some("http://[::1]:8080"), None, true));
        assert!(!origin_allowed(Some("https://evil.example"), Some("evil.example"), true));
        assert!(!origin_allowed(Some("http://localhost.evil.example"), None, true));
        assert!(!origin_allowed(Some("null"), None, true));
        assert!(origin_allowed(Some("http://10.0.0.5:3001"), Some("10.0.0.5:3001"), false));
        assert!(host_allowed(Some("127.0.0.1:3001"), true));
        assert!(!host_allowed(Some("evil.example:3001"), true));
    }

    #[test]
    fn tokens() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert_eq!(bearer_from_header("Bearer xyz"), Some("xyz"));
        assert_eq!(bearer_from_header("Basic xyz"), None);
        assert_eq!(generate_token().len(), 64);
    }
}
