//! Page/WebView lifetime state shared by both desktop platform shells.

use std::collections::{BTreeMap, HashMap};
use std::io;

use interprocess::local_socket::tokio::Stream;
use tokio::sync::mpsc;

use crate::desktop::bridge::{InEnvelope, OutEnvelope, OutKind};
use crate::remote::proto::{ClientFrame, ClientOp, PushEvent, RemoteResponse, Topic};

use super::{GatewayAdmissionError, GatewayEvent, write_line};

/// Latest subscriptions declared by one page lifetime. The page id is part of the value so a
/// replacement WebView invalidates the previous declaration even when it asks for identical
/// topics; the live session then performs a full unsubscribe/subscribe snapshot handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SubscriptionState {
    pub(super) page_id: Option<String>,
    pub(super) topics: Vec<Topic>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FrontendCorrelation {
    pub(super) page_id: Option<String>,
    pub(super) id: u64,
    /// Whether reply loss leaves a state change or dispatch outcome ambiguous. The envelope kind
    /// supplies the initial value; `forward_command` refines it from the parsed remote command.
    pub(super) mutation: bool,
    /// `req`-kind envelopes resolve to the reply's `data` payload (or `null`); `cmd`-kind
    /// envelopes keep the response body with `data`'s members folded in (docs/gui/02 §13).
    pub(super) req: bool,
}

/// A `sub`/`unsub` payload is a JSON array of wire topic strings. Empty is treated as nothing
/// to do (the client already guards this, but be defensive).
pub(super) fn parse_topics(payload: &serde_json::Value) -> Option<Vec<Topic>> {
    let topics: Vec<Topic> = serde_json::from_value(payload.clone()).ok()?;
    (!topics.is_empty()).then_some(topics)
}

pub(super) fn validate_page_id(page_id: Option<&str>) -> Result<(), GatewayAdmissionError> {
    const MAX_PAGE_ID_BYTES: usize = 64;
    if page_id.is_some_and(|page_id| {
        page_id.len() > MAX_PAGE_ID_BYTES || !crate::remote::requests::valid_request_id(page_id)
    }) {
        return Err(GatewayAdmissionError::InvalidPage);
    }
    Ok(())
}

/// Switch to a newly observed page and discard the previous page's desired topics. `None` is the
/// legacy protocol and deliberately cannot identify replacement pages.
pub(super) fn activate_page_state(page_id: Option<&str>, desired: &mut SubscriptionState) -> bool {
    let Some(page_id) = page_id else {
        return false;
    };
    if desired.page_id.as_deref() == Some(page_id) {
        return false;
    }
    desired.page_id = Some(page_id.to_owned());
    desired.topics.clear();
    true
}

/// Fold a subscription declaration into a reconnect-surviving desired-topic snapshot.
/// Order-preserving and deduplicated (the set stays tiny — at most the wire topic enum).
pub(super) fn apply_subscription_change(
    kind: OutKind,
    topics: &[Topic],
    desired: &mut Vec<Topic>,
) -> bool {
    let before = desired.clone();
    match kind {
        OutKind::Sub => {
            for topic in topics {
                if !desired.contains(topic) {
                    desired.push(*topic);
                }
            }
        }
        OutKind::Unsub => desired.retain(|topic| !topics.contains(topic)),
        _ => {}
    }
    *desired != before
}

pub(super) fn correlation(env: &OutEnvelope) -> Option<FrontendCorrelation> {
    env.id
        .filter(|_| matches!(env.kind, OutKind::Req | OutKind::Cmd))
        .map(|id| FrontendCorrelation {
            page_id: env.page_id.clone(),
            id,
            mutation: matches!(env.kind, OutKind::Cmd),
            req: matches!(env.kind, OutKind::Req),
        })
}

/// Bind the frontend's stable identity to its page lifetime before it reaches the core deduper.
/// Legacy envelopes have no page id and retain the exact pre-generation identity behavior.
pub(super) fn request_identity(
    env: &OutEnvelope,
    correlation: Option<&FrontendCorrelation>,
    gateway_namespace: &str,
    session_id: u64,
) -> Result<String, ()> {
    let identity = match env.request_id.as_deref() {
        Some(request_id) if crate::remote::requests::valid_request_id(request_id) => {
            request_id.to_owned()
        }
        Some(_) => return Err(()),
        None => match (env.page_id.as_deref(), correlation) {
            (Some(_), Some(correlation)) => format!("client:{}", correlation.id),
            (Some(_), None) => format!("frame:{session_id}"),
            (None, Some(correlation)) => {
                format!("{gateway_namespace}:client:{}", correlation.id)
            }
            (None, None) => format!("{gateway_namespace}:frame:{session_id}"),
        },
    };
    let identity = match env.page_id.as_deref() {
        Some(page_id) => format!("page:{page_id}:{identity}"),
        None => identity,
    };
    crate::remote::requests::valid_request_id(&identity)
        .then_some(identity)
        .ok_or(())
}

pub(super) fn reject_offline_command<F: Fn(GatewayEvent)>(
    env: OutEnvelope,
    emit: &F,
    reason: &str,
) {
    if let Some(correlation) = correlation(&env) {
        emit(GatewayEvent::Frame(InEnvelope::err_for_page(
            correlation.id,
            correlation.page_id,
            serde_json::json!({ "reason": reason }),
        )));
    } else if matches!(env.kind, OutKind::Cmd | OutKind::Req) {
        tracing::debug!(
            target: "ytt_desktop",
            envelope_kind = ?env.kind,
            envelope_name = %env.name,
            %reason,
            "dropping uncorrelated gateway command while offline"
        );
    }
}

pub(super) fn drain_offline_commands<F: Fn(GatewayEvent)>(
    cmd_rx: &mut mpsc::Receiver<OutEnvelope>,
    emit: &F,
    reason: &str,
) {
    while let Ok(env) = cmd_rx.try_recv() {
        reject_offline_command(env, emit, reason);
    }
}

/// Reconcile one live session from its applied topic set to the latest desired set. A page change
/// deliberately cycles every page-owned topic so the server emits fresh initial snapshots.
pub(super) async fn reconcile_subscriptions(
    conn: &Stream,
    next_id: &mut u64,
    applied: &SubscriptionState,
    desired: &SubscriptionState,
) -> io::Result<Vec<u64>> {
    let applied_topics = initial_topics(&applied.topics);
    let desired_topics = initial_topics(&desired.topics);
    let page_changed = applied.page_id != desired.page_id;
    let removed: Vec<Topic> = applied_topics
        .iter()
        .copied()
        .filter(|topic| {
            *topic != Topic::System && (page_changed || !desired_topics.contains(topic))
        })
        .collect();
    let added: Vec<Topic> = desired_topics
        .iter()
        .copied()
        .filter(|topic| {
            *topic != Topic::System && (page_changed || !applied_topics.contains(topic))
        })
        .collect();
    let mut frame_ids = Vec::new();
    for (page_id, op) in [
        (
            applied.page_id.clone(),
            (!removed.is_empty()).then_some(ClientOp::Unsubscribe { topics: removed }),
        ),
        (
            desired.page_id.clone(),
            (!added.is_empty()).then_some(ClientOp::Subscribe { topics: added }),
        ),
    ] {
        let Some(op) = op else { continue };
        let frame = ClientFrame {
            id: *next_id,
            request_id: None,
            page_id,
            op,
        };
        *next_id += 1;
        write_line(conn, &frame).await?;
        frame_ids.push(frame.id);
    }
    Ok(frame_ids)
}

pub(super) fn event_envelope(topic: Topic, event: &PushEvent) -> InEnvelope {
    let page_id = match event {
        PushEvent::SearchCompleted { page_id, .. } => page_id.clone(),
        _ => None,
    };
    let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    InEnvelope::event_for_page(topic.wire_str(), page_id, payload)
}

pub(super) fn reject_pending<F: Fn(GatewayEvent)>(
    pending: &mut HashMap<u64, FrontendCorrelation>,
    reason: &str,
    emit: &F,
) {
    // A malformed/legacy frontend can reuse one correlation id. Collapse it to one reply while
    // conservatively letting any mutating use dominate the classification.
    let mut correlations = BTreeMap::<(Option<String>, u64), bool>::new();
    for correlation in pending.drain().map(|(_, value)| value) {
        correlations
            .entry((correlation.page_id, correlation.id))
            .and_modify(|mutation| *mutation |= correlation.mutation)
            .or_insert(correlation.mutation);
    }
    for ((page_id, id), mutation) in correlations {
        let failure_reason = if mutation {
            crate::remote::proto::CONFIRMATION_LOST_REASON
        } else {
            reason
        };
        emit(GatewayEvent::Frame(InEnvelope::err_for_page(
            id,
            page_id,
            serde_json::json!({ "reason": failure_reason }),
        )));
    }
}

/// The session-opening subscribe: `system` first (the gateway's own keep-alive/shutdown
/// listener), then every window-declared topic.
pub(super) fn initial_topics(desired: &[Topic]) -> Vec<Topic> {
    let mut topics = vec![Topic::System];
    for topic in desired {
        if !topics.contains(topic) {
            topics.push(*topic);
        }
    }
    topics
}

pub(super) fn reply_envelope(correlation: FrontendCorrelation, resp: RemoteResponse) -> InEnvelope {
    if resp.ok {
        let mut payload = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
        if correlation.req {
            // `req` consumers were built against the demo core's replies: the bare data
            // body, or `null` when there is none (fetch_why_gem on an unknown track).
            payload = match &mut payload {
                serde_json::Value::Object(map) => {
                    map.remove("data").unwrap_or(serde_json::Value::Null)
                }
                _ => serde_json::Value::Null,
            };
        } else if let serde_json::Value::Object(map) = &mut payload {
            // `cmd` consumers read data members off the reply body itself
            // (`payload.conflict`, `payload.cleared`) — fold them in.
            if let Some(serde_json::Value::Object(data)) = map.remove("data") {
                map.extend(data);
            }
        }
        InEnvelope::res_for_page(correlation.id, correlation.page_id, payload)
    } else {
        let mut reason = resp.reason.unwrap_or_else(|| "error".to_string());
        // Protocol-v8 cores report this directly. Normalizing the legacy timeout here keeps a
        // newer desktop shell honest while it is connected to an older compatible owner.
        if correlation.mutation && reason == "timeout" {
            reason = crate::remote::proto::CONFIRMATION_LOST_REASON.to_string();
        }
        InEnvelope::err_for_page(
            correlation.id,
            correlation.page_id,
            serde_json::json!({ "reason": reason }),
        )
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::desktop::bridge::InKind;

    #[test]
    fn reply_envelope_maps_confirmed_rejection_and_lost_mutation_confirmation() {
        let ok = reply_envelope(
            FrontendCorrelation {
                page_id: Some("page-a".to_string()),
                id: 5,
                mutation: false,
                req: false,
            },
            RemoteResponse::ok("done".to_string()),
        );
        assert_eq!(ok.id, Some(5));
        assert_eq!(ok.page_id.as_deref(), Some("page-a"));
        assert_eq!(ok.kind, InKind::Res);

        let bad = reply_envelope(
            FrontendCorrelation {
                page_id: Some("page-b".to_string()),
                id: 6,
                mutation: true,
                req: false,
            },
            RemoteResponse::err("bad_request"),
        );
        assert_eq!(bad.page_id.as_deref(), Some("page-b"));
        assert_eq!(bad.kind, InKind::Err);
        assert_eq!(
            bad.payload,
            Some(serde_json::json!({ "reason": "bad_request" }))
        );

        let legacy_timeout = reply_envelope(
            FrontendCorrelation {
                page_id: Some("page-c".to_string()),
                id: 7,
                mutation: true,
                req: false,
            },
            RemoteResponse::err("timeout"),
        );
        assert_eq!(
            legacy_timeout.payload,
            Some(serde_json::json!({ "reason": "confirmation_lost" }))
        );
    }

    #[test]
    fn req_replies_project_data_or_null_and_cmd_replies_fold_data_in() {
        use crate::remote::proto::ResponseData;
        let req = |id: u64| FrontendCorrelation {
            page_id: Some("page-r".to_string()),
            id,
            mutation: false,
            req: true,
        };
        let cmd = |id: u64| FrontendCorrelation {
            page_id: Some("page-c".to_string()),
            id,
            mutation: true,
            req: false,
        };

        // req + no data → null payload (the demo core's "nothing found" contract).
        let none = reply_envelope(req(1), RemoteResponse::ok("done".to_string()));
        assert_eq!(none.kind, InKind::Res);
        assert_eq!(none.payload, Some(serde_json::Value::Null));

        // req + data → the bare data body, nothing else.
        let mut with_data = RemoteResponse::ok("done".to_string());
        with_data.data = Some(ResponseData::Cleared { cleared: 7 });
        let projected = reply_envelope(req(2), with_data.clone());
        assert_eq!(projected.payload, Some(serde_json::json!({ "cleared": 7 })));

        // cmd + data → response body with data's members folded to the top level.
        let folded = reply_envelope(cmd(3), with_data);
        let payload = folded.payload.expect("cmd reply keeps a body");
        assert_eq!(payload["ok"], serde_json::json!(true));
        assert_eq!(payload["cleared"], serde_json::json!(7));
        assert!(
            payload.get("data").is_none(),
            "folded members must not stay nested: {payload}"
        );

        // cmd + no data → unchanged full body (the shipped shape).
        let plain = reply_envelope(cmd(4), RemoteResponse::ok("done".to_string()));
        let payload = plain.payload.expect("cmd reply keeps a body");
        assert_eq!(payload["ok"], serde_json::json!(true));
        assert_eq!(payload["message"], serde_json::json!("done"));
    }

    #[test]
    fn replacement_page_resets_even_an_identical_topic_declaration() {
        let mut state = SubscriptionState {
            page_id: Some("page-a".to_string()),
            topics: vec![Topic::Player, Topic::Queue],
        };
        assert!(activate_page_state(Some("page-b"), &mut state));
        assert_eq!(state.page_id.as_deref(), Some("page-b"));
        assert!(state.topics.is_empty());
        assert!(!activate_page_state(Some("page-b"), &mut state));
    }

    #[test]
    fn legacy_page_id_remains_additive_and_optional() {
        let mut state = SubscriptionState {
            page_id: None,
            topics: vec![Topic::Player],
        };
        assert!(validate_page_id(None).is_ok());
        assert!(!activate_page_state(None, &mut state));
        assert_eq!(state.topics, vec![Topic::Player]);
    }

    #[test]
    fn request_identity_is_page_scoped_but_legacy_identity_is_preserved() {
        let page = OutEnvelope {
            v: 1,
            id: Some(1),
            page_id: Some("page-a".to_string()),
            request_id: Some("gui:page-a:1".to_string()),
            kind: OutKind::Req,
            name: "status".to_string(),
            payload: serde_json::Value::Null,
        };
        let page_correlation = correlation(&page).unwrap();
        assert_eq!(
            request_identity(&page, Some(&page_correlation), "gateway", 7).as_deref(),
            Ok("page:page-a:gui:page-a:1")
        );

        let legacy = OutEnvelope {
            page_id: None,
            request_id: Some("legacy-stable-id".to_string()),
            ..page
        };
        let legacy_correlation = correlation(&legacy).unwrap();
        assert_eq!(
            request_identity(&legacy, Some(&legacy_correlation), "gateway", 8).as_deref(),
            Ok("legacy-stable-id")
        );
    }

    #[test]
    fn search_event_envelope_targets_the_requesting_page() {
        let event = PushEvent::SearchCompleted {
            ticket: 1,
            page_id: Some("page-a".to_string()),
            query: "query".to_string(),
            source: crate::search_source::SearchSource::All,
            groups: Vec::new(),
        };

        let envelope = event_envelope(Topic::Search, &event);
        assert_eq!(envelope.page_id.as_deref(), Some("page-a"));
        assert_eq!(envelope.topic.as_deref(), Some("search"));
        assert_eq!(envelope.payload.as_ref().unwrap()["page_id"], "page-a");
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use interprocess::local_socket::tokio::Listener;
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions};
    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    #[tokio::test]
    async fn common_window_replacement_cycles_topics_for_full_resnapshot() {
        let nonce = crate::remote::requests::fresh_request_id();
        let endpoint = std::env::temp_dir()
            .join(format!("ygp-{}-{}.sock", std::process::id(), &nonce[..8]))
            .to_string_lossy()
            .into_owned();
        let name = endpoint.as_str().to_fs_name::<GenericFilePath>().unwrap();
        let listener: Listener = ListenerOptions::new().name(name).create_tokio().unwrap();
        let name = endpoint.as_str().to_fs_name::<GenericFilePath>().unwrap();
        let conn = Stream::connect(name).await.unwrap();
        let server = tokio::spawn(async move {
            let peer = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&peer);
            let mut frames = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                frames.push(serde_json::from_str::<ClientFrame>(line.trim()).unwrap());
            }
            frames
        });

        let old = SubscriptionState {
            page_id: Some("page-a".to_string()),
            topics: vec![Topic::Player, Topic::Queue],
        };
        let replacement = SubscriptionState {
            page_id: Some("page-b".to_string()),
            topics: vec![Topic::Player, Topic::Queue],
        };
        let mut next_id = 10;
        let frame_ids = reconcile_subscriptions(&conn, &mut next_id, &old, &replacement)
            .await
            .unwrap();

        let frames = server.await.unwrap();
        assert_eq!(next_id, 12);
        assert_eq!(frame_ids, vec![10, 11]);
        assert_eq!(frames[0].page_id.as_deref(), Some("page-a"));
        assert_eq!(
            frames[0].op,
            ClientOp::Unsubscribe {
                topics: vec![Topic::Player, Topic::Queue]
            }
        );
        assert_eq!(frames[1].page_id.as_deref(), Some("page-b"));
        assert_eq!(
            frames[1].op,
            ClientOp::Subscribe {
                topics: vec![Topic::Player, Topic::Queue]
            }
        );
        let _ = std::fs::remove_file(endpoint);
    }
}
