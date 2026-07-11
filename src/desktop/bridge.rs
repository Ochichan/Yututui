//! WebView ↔ gateway glue (docs/gui/03 §2, §3.2, docs/gui/05 §4.1).
//!
//! The webview posts [`OutEnvelope`] lines via wry's ipc_handler (on the main thread); the
//! bridge parses them into a [`BridgeAction`] the event loop executes: a local reply
//! (the M0 ping echo), a native window op, or (from M1) a frame forwarded to the gateway.
//! Outbound, [`receive_script`] renders an [`InEnvelope`] into a `window.__ytm.receive(...)`
//! call the loop thread runs with `evaluate_script` (WebViews are `!Send`, docs/gui/03 §3.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const BRIDGE_VERSION: u8 = 1;
pub const MAX_BRIDGE_MESSAGE_BYTES: usize = 64 * 1024;
/// Upper half of u64 is reserved for native shell correlations and can never originate in a
/// WebView request. Keeping the boundary here lets parsing reject collisions before routing.
pub const MAX_PAGE_REQUEST_ID: u64 = (1 << 63) - 1;

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
            v: BRIDGE_VERSION,
            id: Some(id),
            kind: InKind::Res,
            topic: None,
            payload: Some(payload),
        }
    }
    pub fn err(id: u64, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
            id: Some(id),
            kind: InKind::Err,
            topic: None,
            payload: Some(payload),
        }
    }
    pub fn conn(payload: serde_json::Value) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
            id: None,
            kind: InKind::Conn,
            topic: None,
            payload: Some(payload),
        }
    }
    pub fn event(topic: &str, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
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
    /// The main frontend installed its receive handler and subscriptions; replay current truth.
    FrontendReady,
    CopyText(String),
    OpenUrl(String),
    StartDaemon,
    /// Small, non-authoritative frontend state retained across idle WebView teardown.
    PersistUi(UiSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiView {
    Now,
    Search,
    Library,
    Settings,
    Ai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSettingsTab {
    General,
    Playback,
    Hotkeys,
    Graphics,
    Djgem,
    Accounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiLibraryTab {
    All,
    Favorites,
    History,
    Downloads,
    Playlists,
    RadioLikes,
    RadioHistory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiSnapshot {
    pub view: UiView,
    pub queue_open: bool,
    pub settings_tab: UiSettingsTab,
    pub library_tab: UiLibraryTab,
    pub scroll_y: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_control: Option<String>,
    /// Scroll offsets of explicitly named, page-local scroll containers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scroll_positions: BTreeMap<String, u32>,
    /// Non-sensitive, explicitly opted-in text drafts retained only in host memory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drafts: BTreeMap<String, String>,
}

impl UiSnapshot {
    const MAX_SCROLL_Y: u32 = 10_000_000;
    const MAX_ACTIVE_CONTROL_BYTES: usize = 128;
    const MAX_SCROLL_POSITIONS: usize = 16;
    const MAX_SCROLL_KEY_BYTES: usize = 64;
    const MAX_DRAFTS: usize = 8;
    const MAX_DRAFT_KEY_BYTES: usize = 64;
    const MAX_DRAFT_VALUE_BYTES: usize = 4 * 1024;

    fn is_bounded(&self) -> bool {
        self.scroll_y <= Self::MAX_SCROLL_Y
            && self
                .active_control
                .as_ref()
                .is_none_or(|id| id.len() <= Self::MAX_ACTIVE_CONTROL_BYTES)
            && self.scroll_positions.len() <= Self::MAX_SCROLL_POSITIONS
            && self.scroll_positions.iter().all(|(key, offset)| {
                !key.is_empty()
                    && key.len() <= Self::MAX_SCROLL_KEY_BYTES
                    && *offset <= Self::MAX_SCROLL_Y
            })
            && self.drafts.len() <= Self::MAX_DRAFTS
            && self.drafts.iter().all(|(key, value)| {
                !key.is_empty()
                    && key.len() <= Self::MAX_DRAFT_KEY_BYTES
                    && value.len() <= Self::MAX_DRAFT_VALUE_BYTES
            })
    }
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
    if body.len() > MAX_BRIDGE_MESSAGE_BYTES {
        return BridgeAction::Ignore;
    }
    let Ok(env) = serde_json::from_str::<OutEnvelope>(body) else {
        return BridgeAction::Ignore;
    };
    if env.v != BRIDGE_VERSION {
        return env.id.map_or(BridgeAction::Ignore, |id| {
            BridgeAction::Reply(InEnvelope::err(
                id,
                serde_json::json!({
                    "code": "unsupported_version",
                    "expected": BRIDGE_VERSION,
                    "received": env.v,
                }),
            ))
        });
    }
    if env.id.is_some_and(|id| id > MAX_PAGE_REQUEST_ID) {
        return env.id.map_or(BridgeAction::Ignore, |id| {
            BridgeAction::Reply(InEnvelope::err(
                id,
                serde_json::json!({ "code": "invalid_request_id" }),
            ))
        });
    }
    match env.kind {
        // M0 self-test: the IPC bridge echoes `req ping` → `res pong` locally (docs/gui/09 §3).
        OutKind::Req if env.name == "ping" => match env.id {
            Some(id) => BridgeAction::Reply(InEnvelope::res(id, serde_json::json!("pong"))),
            None => BridgeAction::Ignore,
        },
        OutKind::Win => parse_win(&env).map_or(BridgeAction::Ignore, BridgeAction::Win),
        // Everything else belongs to the gateway once it lands (M1); harmless to forward now.
        OutKind::Cmd | OutKind::Req | OutKind::Sub | OutKind::Unsub => BridgeAction::ToGateway(env),
    }
}

fn parse_win(env: &OutEnvelope) -> Option<WinOp> {
    let text = || {
        env.payload
            .as_str()
            .or_else(|| env.payload.get("text").and_then(|value| value.as_str()))
            .unwrap_or_default()
            .to_string()
    };
    let url = || {
        env.payload
            .as_str()
            .or_else(|| env.payload.get("url").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string()
    };
    match env.name.as_str() {
        "drag" => Some(WinOp::Drag),
        "hide" => Some(WinOp::Hide),
        "frontendReady" => Some(WinOp::FrontendReady),
        "copyText" => Some(WinOp::CopyText(text())),
        "openUrl" => Some(WinOp::OpenUrl(url())),
        "startDaemon" => Some(WinOp::StartDaemon),
        "persistUi" => serde_json::from_value::<UiSnapshot>(env.payload.clone())
            .ok()
            .filter(UiSnapshot::is_bounded)
            .map(WinOp::PersistUi),
        // Do not advertise or parse operations the native adapters cannot honor.
        _ => None,
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
    fn unsupported_bridge_versions_are_rejected_before_dispatch() {
        let action = dispatch(r#"{"v":2,"id":9,"kind":"req","name":"ping"}"#);
        match action {
            BridgeAction::Reply(env) => {
                assert_eq!(env.id, Some(9));
                assert_eq!(env.kind, InKind::Err);
                assert_eq!(
                    env.payload,
                    Some(serde_json::json!({
                        "code": "unsupported_version",
                        "expected": 1,
                        "received": 2,
                    }))
                );
            }
            other => panic!("expected version error, got {other:?}"),
        }
        assert_eq!(
            dispatch(r#"{"v":0,"kind":"cmd","name":"next"}"#),
            BridgeAction::Ignore
        );
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
            dispatch(r#"{"v":1,"kind":"win","name":"frontendReady"}"#),
            BridgeAction::Win(WinOp::FrontendReady)
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"copyText","payload":"hello"}"#),
            BridgeAction::Win(WinOp::CopyText("hello".to_string()))
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"copyText","payload":{"text":"hello"}}"#),
            BridgeAction::Win(WinOp::CopyText("hello".to_string()))
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"openUrl","payload":"https://example.test"}"#),
            BridgeAction::Win(WinOp::OpenUrl("https://example.test".to_string()))
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"openDevtools"}"#),
            BridgeAction::Ignore
        );
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"win","name":"persist","payload":{}}"#),
            BridgeAction::Ignore
        );
        assert_eq!(
            dispatch(
                r#"{"v":1,"kind":"win","name":"persistUi","payload":{"view":"settings","queueOpen":false,"settingsTab":"playback","libraryTab":"favorites","scrollY":420,"activeControl":"speed"}}"#
            ),
            BridgeAction::Win(WinOp::PersistUi(UiSnapshot {
                view: UiView::Settings,
                queue_open: false,
                settings_tab: UiSettingsTab::Playback,
                library_tab: UiLibraryTab::Favorites,
                scroll_y: 420,
                active_control: Some("speed".to_string()),
                scroll_positions: BTreeMap::new(),
                drafts: BTreeMap::new(),
            }))
        );
        assert_eq!(
            dispatch(
                r#"{"v":1,"kind":"win","name":"persistUi","payload":{"view":"bogus","queueOpen":false,"settingsTab":"playback","libraryTab":"favorites","scrollY":0}}"#
            ),
            BridgeAction::Ignore
        );
        assert_eq!(
            dispatch(
                r#"{"v":1,"kind":"win","name":"persistUi","payload":{"view":"search","queueOpen":true,"settingsTab":"general","libraryTab":"all","scrollY":0,"scrollPositions":{"search-results":123},"drafts":{"search-query":"cats"}}}"#
            ),
            BridgeAction::Win(WinOp::PersistUi(UiSnapshot {
                view: UiView::Search,
                queue_open: true,
                settings_tab: UiSettingsTab::General,
                library_tab: UiLibraryTab::All,
                scroll_y: 0,
                active_control: None,
                scroll_positions: BTreeMap::from([("search-results".to_string(), 123)]),
                drafts: BTreeMap::from([("search-query".to_string(), "cats".to_string())]),
            }))
        );
        let too_many_scroll_positions = (0..17)
            .map(|index| format!(r#""scroll-{index}":0"#))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            dispatch(&format!(
                r#"{{"v":1,"kind":"win","name":"persistUi","payload":{{"view":"now","queueOpen":true,"settingsTab":"general","libraryTab":"all","scrollY":0,"scrollPositions":{{{too_many_scroll_positions}}}}}}}"#
            )),
            BridgeAction::Ignore
        );
        assert_eq!(
            dispatch(
                r#"{"v":1,"kind":"win","name":"openUrl","payload":{"url":"https://example.test"}}"#
            ),
            BridgeAction::Win(WinOp::OpenUrl("https://example.test".to_string()))
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
    fn webview_cannot_claim_the_native_request_id_range() {
        let action = dispatch(r#"{"v":1,"id":9223372036854775808,"kind":"req","name":"status"}"#);
        match action {
            BridgeAction::Reply(env) => {
                assert_eq!(env.id, Some(1 << 63));
                assert_eq!(env.kind, InKind::Err);
                assert_eq!(
                    env.payload,
                    Some(serde_json::json!({ "code": "invalid_request_id" }))
                );
            }
            other => panic!("expected request-id error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_is_ignored() {
        assert_eq!(dispatch("{not json}"), BridgeAction::Ignore);
        assert_eq!(
            dispatch(r#"{"v":1,"kind":"bogus","name":"x"}"#),
            BridgeAction::Ignore
        );
        let oversized = format!(
            r#"{{"v":1,"kind":"win","name":"copyText","payload":"{}"}}"#,
            "x".repeat(MAX_BRIDGE_MESSAGE_BYTES)
        );
        assert_eq!(dispatch(&oversized), BridgeAction::Ignore);
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
