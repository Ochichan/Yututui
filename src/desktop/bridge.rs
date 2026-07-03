//! WebView ↔ gateway glue (docs/gui/03 §2, §3.2, docs/gui/05 §4.1).
//!
//! The webview posts [`OutEnvelope`] lines via wry's ipc_handler (on the main thread); the
//! bridge parses them into a [`BridgeAction`] the event loop executes: a local reply
//! (the M0 ping echo), a native window op, or (from M1) a frame forwarded to the gateway.
//! Outbound, [`receive_script`] renders an [`InEnvelope`] into a `window.__ytm.receive(...)`
//! call the loop thread runs with `evaluate_script` (WebViews are `!Send`, docs/gui/03 §3.2).

use serde::{Deserialize, Serialize};

/// Frontend → Rust. Mirrors the TS `OutEnvelope` (docs/gui/05 §4.1).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutEnvelope {
    pub v: u8,
    #[serde(default)]
    pub id: Option<u64>,
    pub kind: OutKind,
    pub name: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutKind {
    Cmd,
    Req,
    Sub,
    Unsub,
    Win,
}

/// Rust → frontend. Mirrors the TS `InEnvelope`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InEnvelope {
    pub v: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub kind: InKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InKind {
    Res,
    Err,
    Event,
    Conn,
}

impl InEnvelope {
    pub fn res(id: u64, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: 1,
            id: Some(id),
            kind: InKind::Res,
            topic: None,
            payload: Some(payload),
        }
    }
    pub fn conn(payload: serde_json::Value) -> Self {
        InEnvelope {
            v: 1,
            id: None,
            kind: InKind::Conn,
            topic: None,
            payload: Some(payload),
        }
    }
    pub fn event(topic: &str, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: 1,
            id: None,
            kind: InKind::Event,
            topic: Some(topic.to_string()),
            payload: Some(payload),
        }
    }
}

/// A native window op the tao loop performs — never routed to the gateway (docs/gui/05 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinOp {
    Drag,
    Hide,
    OpenDevtools,
    CopyText(String),
    OpenUrl(String),
    StartDaemon,
    /// The frontend's persist() reply carrying a cached UiSnapshot (M1+); opaque at M0.
    Persist(String),
    Unknown(String),
}

/// What the event loop should do with an inbound webview message.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeAction {
    /// Evaluate a local reply on the source webview (e.g. the ping echo).
    Reply(InEnvelope),
    /// Perform a native window op.
    Win(WinOp),
    /// Forward to the gateway thread (commands/requests/subscriptions — M1+).
    ToGateway(OutEnvelope),
    /// Malformed or intentionally ignored.
    Ignore,
}

/// Parse and classify one webview IPC message.
pub fn dispatch(body: &str) -> BridgeAction {
    let Ok(env) = serde_json::from_str::<OutEnvelope>(body) else {
        return BridgeAction::Ignore;
    };
    match env.kind {
        // M0 self-test: the IPC bridge echoes `req ping` → `res pong` locally (docs/gui/09 §3).
        OutKind::Req if env.name == "ping" => match env.id {
            Some(id) => BridgeAction::Reply(InEnvelope::res(id, serde_json::json!("pong"))),
            None => BridgeAction::Ignore,
        },
        OutKind::Win => BridgeAction::Win(parse_win(&env)),
        // Everything else belongs to the gateway once it lands (M1); harmless to forward now.
        OutKind::Cmd | OutKind::Req | OutKind::Sub | OutKind::Unsub => BridgeAction::ToGateway(env),
    }
}

fn parse_win(env: &OutEnvelope) -> WinOp {
    let text = || env.payload.as_str().unwrap_or_default().to_string();
    match env.name.as_str() {
        "drag" => WinOp::Drag,
        "hide" => WinOp::Hide,
        "openDevtools" => WinOp::OpenDevtools,
        "copyText" => WinOp::CopyText(text()),
        "openUrl" => WinOp::OpenUrl(text()),
        "startDaemon" => WinOp::StartDaemon,
        "persist" => WinOp::Persist(env.payload.to_string()),
        other => WinOp::Unknown(other.to_string()),
    }
}

/// Render an inbound envelope into a `window.__ytm.receive(...)` call for `evaluate_script`.
/// The JSON is double-encoded into a JS string literal; U+2028/2029 (valid JSON, illegal in
/// JS string literals) are escaped, mirroring `panel.rs`'s `json_for_script`.
pub fn receive_script(env: &InEnvelope) -> String {
    let json = serde_json::to_string(env).unwrap_or_else(|_| "null".to_string());
    let literal = serde_json::to_string(&json)
        .unwrap_or_else(|_| "\"null\"".to_string())
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!("window.__ytm && window.__ytm.receive({literal});")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_req_echoes_pong() {
        let action = dispatch(r#"{"v":1,"id":7,"kind":"req","name":"ping"}"#);
        match action {
            BridgeAction::Reply(env) => {
                assert_eq!(env.id, Some(7));
                assert_eq!(env.kind, InKind::Res);
                assert_eq!(env.payload, Some(serde_json::json!("pong")));
            }
            other => panic!("expected reply, got {other:?}"),
        }
    }

    #[test]
    fn win_ops_parse() {
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"hide"}"#),
            BridgeAction::Win(WinOp::Hide)
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"drag"}"#),
            BridgeAction::Win(WinOp::Drag)
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"startDaemon"}"#),
            BridgeAction::Win(WinOp::StartDaemon)
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"copyText","payload":"hello"}"#),
            BridgeAction::Win(WinOp::CopyText("hello".to_string()))
        );
    }

    #[test]
    fn non_ping_req_and_cmd_route_to_gateway() {
        assert!(matches!(
            dispatch(r#"{"v":1,"id":2,"kind":"req","name":"fetch"}"#),
            BridgeAction::ToGateway(_)
        ));
        assert!(matches!(
            dispatch(r#"{"v":1,"kind":"cmd","name":"toggle_pause"}"#),
            BridgeAction::ToGateway(_)
        ));
    }

    #[test]
    fn malformed_is_ignored() {
        assert_eq!(dispatch("{not json}"), BridgeAction::Ignore);
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"bogus","name":"x"}"#),
            BridgeAction::Ignore
        );
    }

    #[test]
    fn receive_script_is_a_safe_js_string_literal() {
        let env = InEnvelope::conn(serde_json::json!({"state":"online"}));
        let script = receive_script(&env);
        assert!(script.starts_with("window.__ytm && window.__ytm.receive(\""));
        assert!(script.ends_with("\");"));
        // The payload JSON is embedded as an escaped string, not a bare object.
        assert!(script.contains("\\\"state\\\""));
    }

    #[test]
    fn receive_script_escapes_js_line_separators() {
        let env = InEnvelope::event("system", serde_json::json!({"msg":"a\u{2028}b"}));
        let script = receive_script(&env);
        assert!(script.contains("\\u2028"));
        assert!(!script.contains('\u{2028}'));
    }
}
