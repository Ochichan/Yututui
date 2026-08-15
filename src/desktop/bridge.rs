//! Gateway envelope types shared by the tray companion's session lane.
//!
//! [`OutEnvelope`] frames (commands/requests/subscriptions) are constructed natively and
//! written to the v8 session; inbound server frames arrive as [`InEnvelope`]s the owner
//! loop consumes in Rust. The page/generation fields fence stale WebView lifetimes: a
//! replacement page must never accept an older page's correlated reply.

use serde::{Deserialize, Serialize};

pub const BRIDGE_VERSION: u8 = 1;
/// Upper half of u64 is reserved for native shell correlations and can never originate in a
/// WebView request. Keeping the boundary here lets parsing reject collisions before routing.
pub const MAX_PAGE_REQUEST_ID: u64 = (1 << 63) - 1;

/// One gateway-bound frame: a command, a correlated request, or a subscription change.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutEnvelope {
    pub v: u8,
    #[serde(default)]
    pub id: Option<u64>,
    /// Page/WebView lifetime namespace. Older clients omit it and retain legacy behavior.
    #[serde(default)]
    pub page_id: Option<String>,
    /// Stable mutation identity, distinct from the page-local response correlation id.
    #[serde(default)]
    pub request_id: Option<String>,
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
}

/// Rust-side view of one inbound session frame (reply or subscribed push).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InEnvelope {
    pub v: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// Echoed on correlated replies so a replacement page rejects an older page's response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
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
}

impl InEnvelope {
    pub fn res_for_page(id: u64, page_id: Option<String>, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
            id: Some(id),
            page_id,
            kind: InKind::Res,
            topic: None,
            payload: Some(payload),
        }
    }

    pub fn err_for_page(id: u64, page_id: Option<String>, payload: serde_json::Value) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
            id: Some(id),
            page_id,
            kind: InKind::Err,
            topic: None,
            payload: Some(payload),
        }
    }

    pub fn event_for_page(
        topic: &str,
        page_id: Option<String>,
        payload: serde_json::Value,
    ) -> Self {
        InEnvelope {
            v: BRIDGE_VERSION,
            id: None,
            page_id,
            kind: InKind::Event,
            topic: Some(topic.to_string()),
            payload: Some(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_envelopes_carry_the_page_namespace() {
        let res =
            InEnvelope::res_for_page(7, Some("page-a".to_string()), serde_json::json!("pong"));
        assert_eq!(res.id, Some(7));
        assert_eq!(res.page_id.as_deref(), Some("page-a"));
        assert_eq!(res.kind, InKind::Res);
        assert_eq!(res.payload, Some(serde_json::json!("pong")));

        let err = InEnvelope::err_for_page(
            9,
            Some("page-b".to_string()),
            serde_json::json!({ "reason": "stale_page" }),
        );
        let line = serde_json::to_string(&err).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"id":9,"page_id":"page-b","kind":"err","payload":{"reason":"stale_page"}}"#
        );
        let back: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["kind"], "err");
    }

    #[test]
    fn event_envelope_carries_topic_and_page() {
        let env = InEnvelope::event_for_page(
            "player",
            None,
            serde_json::json!({ "kind": "player_snapshot" }),
        );
        assert_eq!(env.kind, InKind::Event);
        assert_eq!(env.topic.as_deref(), Some("player"));
        assert_eq!(env.page_id, None);
        let line = serde_json::to_string(&env).unwrap();
        assert!(line.contains(r#""topic":"player""#));
        assert!(
            !line.contains("page_id"),
            "absent page omits the key: {line}"
        );
    }

    #[test]
    fn out_envelope_parses_and_defaults_optional_fields() {
        let env: OutEnvelope =
            serde_json::from_str(r#"{"v":1,"kind":"cmd","name":"toggle_pause"}"#).unwrap();
        assert_eq!(env.v, BRIDGE_VERSION);
        assert_eq!(env.kind, OutKind::Cmd);
        assert_eq!(env.name, "toggle_pause");
        assert_eq!(env.id, None);
        assert_eq!(env.page_id, None);
        assert_eq!(env.request_id, None);
    }
}
