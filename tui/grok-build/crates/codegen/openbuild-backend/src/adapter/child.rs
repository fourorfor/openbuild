//! Expand the kernel's child-session updates into grok's subagent model.
//!
//! The kernel reports a spawned `task` subagent through two things:
//!
//! - `opencode/session/child_update` ext-notifications, carrying either the
//!   child's own `SessionUpdate` (`type: "update"`) or a lifecycle transition
//!   (`type: "status"`);
//! - a `_meta["opencode/child-session"]` stamp on the projected update.
//!
//! grok knows nothing about that method. It builds a subagent view from
//! `x.ai/session_notification` → `subagent_spawned`, renders the child's
//! transcript from ordinary `session/update` notifications whose `sessionId`
//! is the *child's*, and closes the view on `subagent_finished`.
//!
//! So this module re-keys the stream instead of teaching grok a second
//! protocol. No kernel change is involved: the `child-session-updates`
//! capability the TUI already opts into is the only switch.

use std::collections::{HashMap, HashSet};

use agent_client_protocol as acp;
use serde_json::{Map, Value, json};
use xai_acp_lib::{AcpArgs, AcpClientMessage};

/// Ext method the kernel uses for child-session traffic.
pub const METHOD: &str = "opencode/session/child_update";

/// Ext method grok renders `subagent_spawned` / `subagent_finished` from.
const SESSION_NOTIFICATION: &str = "x.ai/session_notification";

/// One child session as the kernel described it.
#[derive(Debug, Clone, Default)]
struct Child {
    root_session_id: String,
    parent_session_id: String,
    depth: u32,
    title: Option<String>,
    description: String,
}

/// Tracks which children have been announced so the spawn block is emitted
/// exactly once, and so a late `status` update can still finish a child whose
/// spawn message was dropped by a reconnect.
#[derive(Debug, Default)]
pub struct ChildRegistry {
    children: HashMap<String, Child>,
    spawned: HashSet<String>,
    finished: HashSet<String>,
}

impl ChildRegistry {
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Forget every child. Called when the kernel is replaced: the new process
    /// re-announces its children, and stale ids must not suppress that.
    pub fn clear(&mut self) {
        self.children.clear();
        self.spawned.clear();
        self.finished.clear();
    }

    /// `true` when this message is a child update this module owns.
    pub fn handles(message: &AcpClientMessage) -> bool {
        matches!(
            message,
            AcpClientMessage::ExtNotification(args) if args.request.method.as_ref() == METHOD
        )
    }

    /// Expand one child update into grok-renderable messages.
    ///
    /// The inbound notification's response sender is attached to the first
    /// message. Any further message gets a sender whose receiver is already
    /// dropped: the handler's `send(Ok(()))` then fails silently, which is the
    /// correct outcome for a JSON-RPC notification with no id.
    pub fn expand(&mut self, args: AcpArgs<acp::ExtNotification>) -> Vec<AcpClientMessage> {
        let AcpArgs {
            request,
            response_tx,
        } = args;
        let mut reply = ReplySink::new(response_tx);
        let payload = match serde_json::from_str::<Payload>(request.params.get()) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(%error, "ignoring malformed child session update");
                return ack_only(request, &mut reply);
            }
        };
        let key = payload.child_session_id.clone();
        if key.is_empty() {
            return ack_only(request, &mut reply);
        }
        self.remember(&key, &payload);

        let mut out = Vec::new();
        match payload.kind.as_str() {
            "update" => {
                self.push_spawn(&mut out, &key, &payload, &mut reply);
                if let Some(update) = payload.update
                    && let Ok(update) = serde_json::from_value::<acp::SessionUpdate>(update)
                {
                    out.push(child_message(
                        &payload.child_session_id,
                        update,
                        reply.next_or_drop(),
                    ));
                }
            }
            "status" => match payload.status.as_deref() {
                Some("created") | Some("running") => {
                    self.push_spawn(&mut out, &key, &payload, &mut reply);
                }
                Some(status @ ("completed" | "failed" | "interrupted")) => {
                    self.push_spawn(&mut out, &key, &payload, &mut reply);
                    self.push_finish(&mut out, &key, &payload, status, &mut reply);
                }
                Some(other) => {
                    tracing::debug!(status = other, "unknown child session status");
                }
                None => {}
            },
            other => {
                tracing::debug!(kind = other, "unknown child session update kind");
            }
        }
        if out.is_empty() {
            return ack_only(request, &mut reply);
        }
        debug_assert!(
            out.len() <= 3,
            "one child update expands to at most spawn + update + finish"
        );
        out
    }

    fn remember(&mut self, key: &str, payload: &Payload) {
        let entry = self.children.entry(key.to_owned()).or_default();
        if !payload.root_session_id.is_empty() {
            entry.root_session_id = payload.root_session_id.clone();
        }
        if !payload.parent_session_id.is_empty() {
            entry.parent_session_id = payload.parent_session_id.clone();
        }
        if payload.depth > entry.depth {
            entry.depth = payload.depth;
        }
        if let Some(title) = payload.title.as_ref().filter(|t| !t.is_empty()) {
            entry.description = title.clone();
            entry.title = Some(title.clone());
        }
    }

    fn push_spawn(
        &mut self,
        out: &mut Vec<AcpClientMessage>,
        key: &str,
        payload: &Payload,
        reply: &mut ReplySink,
    ) {
        if !self.spawned.insert(key.to_owned()) {
            return;
        }
        let Some(child) = self.children.get(key).cloned() else {
            return;
        };
        // The spawn notification lives on the *root* session's channel: grok
        // resolves the owning agent from it and then creates the child view
        // under `child_session_id`.
        let root = if child.root_session_id.is_empty() {
            payload.parent_session_id.clone()
        } else {
            child.root_session_id.clone()
        };
        let parent = if child.parent_session_id.is_empty() {
            root.clone()
        } else {
            child.parent_session_id.clone()
        };
        let description = if child.description.is_empty() {
            "task".to_owned()
        } else {
            child.description.clone()
        };
        let update = json!({
            "sessionUpdate": "subagent_spawned",
            "subagent_id": key,
            "parent_session_id": parent,
            "child_session_id": key,
            "subagent_type": "task",
            "description": description,
            "context_normalized": false,
            "depth": child.depth,
        });
        if let Some(message) = session_notification(&root, update, reply.next_or_drop()) {
            out.push(message);
        }
    }

    fn push_finish(
        &mut self,
        out: &mut Vec<AcpClientMessage>,
        key: &str,
        payload: &Payload,
        status: &str,
        reply: &mut ReplySink,
    ) {
        if !self.finished.insert(key.to_owned()) {
            return;
        }
        let Some(child) = self.children.get(key).cloned() else {
            return;
        };
        let root = if child.root_session_id.is_empty() {
            payload.parent_session_id.clone()
        } else {
            child.root_session_id.clone()
        };
        let update = json!({
            "sessionUpdate": "subagent_finished",
            "subagent_id": key,
            "child_session_id": key,
            "status": status,
            "error": payload.error.as_ref().and_then(|error| error.message.clone()),
            "tool_calls": 0,
            "turns": 0,
            "duration_ms": 0,
            "tokens_used": 0,
            "will_wake": false,
        });
        if let Some(message) = session_notification(&root, update, reply.next_or_drop()) {
            out.push(message);
        }
    }
}

/// Hands out the `oneshot` senders a batch of expanded messages needs.
///
/// The inbound notification owns one sender. Every additional message gets a
/// sender whose receiver is already dropped: the handler's `send(Ok(()))`
/// fails silently, which is the correct outcome for a JSON-RPC notification.
pub struct ReplySink {
    primary: Option<tokio::sync::oneshot::Sender<acp::Result<()>>>,
}

impl ReplySink {
    pub fn new(primary: tokio::sync::oneshot::Sender<acp::Result<()>>) -> Self {
        Self {
            primary: Some(primary),
        }
    }

    fn next(&mut self) -> Option<tokio::sync::oneshot::Sender<acp::Result<()>>> {
        self.primary.take()
    }

    fn next_or_drop(&mut self) -> tokio::sync::oneshot::Sender<acp::Result<()>> {
        self.primary.take().unwrap_or_else(|| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            tx
        })
    }
}

/// Answer a notification that produced nothing renderable.
fn ack_only(request: acp::ExtNotification, reply: &mut ReplySink) -> Vec<AcpClientMessage> {
    match reply.next() {
        Some(tx) => vec![AcpClientMessage::ExtNotification(AcpArgs {
            request,
            response_tx: tx,
        })],
        None => Vec::new(),
    }
}

fn child_message(
    child_session_id: &str,
    update: acp::SessionUpdate,
    response_tx: tokio::sync::oneshot::Sender<acp::Result<()>>,
) -> AcpClientMessage {
    AcpClientMessage::SessionNotification(AcpArgs {
        request: acp::SessionNotification::new(
            acp::SessionId::new(child_session_id.to_owned()),
            update,
        ),
        response_tx,
    })
}

fn session_notification(
    session_id: &str,
    update: Value,
    response_tx: tokio::sync::oneshot::Sender<acp::Result<()>>,
) -> Option<AcpClientMessage> {
    let payload = json!({ "sessionId": session_id, "update": update });
    let raw = serde_json::value::to_raw_value(&payload).ok()?;
    Some(AcpClientMessage::ExtNotification(AcpArgs {
        request: acp::ExtNotification::new(SESSION_NOTIFICATION, raw.into()),
        response_tx,
    }))
}

#[derive(Debug, Default, serde::Deserialize)]
struct ChildError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    #[serde(default)]
    root_session_id: String,
    #[serde(default)]
    child_session_id: String,
    #[serde(default)]
    parent_session_id: String,
    #[serde(default)]
    depth: u32,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    update: Option<Value>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<ChildError>,
}

/// Build the `_meta` a child's own updates carry, for tests and for callers
/// that want to recognize a child without the registry.
pub fn child_meta(payload: &Map<String, Value>) -> Option<(String, String)> {
    let meta = payload.get("opencode/child-session")?.as_object()?;
    let id = meta.get("id")?.as_str()?.to_owned();
    let parent = meta.get("parentID")?.as_str()?.to_owned();
    Some((id, parent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn ext(payload: Value) -> AcpArgs<acp::ExtNotification> {
        let raw = serde_json::value::to_raw_value(&payload).expect("payload");
        let (tx, _rx) = oneshot::channel();
        AcpArgs {
            request: acp::ExtNotification::new(METHOD, raw.into()),
            response_tx: tx,
        }
    }

    fn expand(registry: &mut ChildRegistry, payload: Value) -> Vec<AcpClientMessage> {
        registry.expand(ext(payload))
    }

    fn update_method(message: &AcpClientMessage) -> Option<String> {
        match message {
            AcpClientMessage::ExtNotification(args) => {
                Some(args.request.method.as_ref().to_owned())
            }
            _ => None,
        }
    }

    fn session_update_name(message: &AcpClientMessage) -> Option<String> {
        let AcpClientMessage::ExtNotification(args) = message else {
            return None;
        };
        let payload: Value = serde_json::from_str(args.request.params.get()).ok()?;
        payload["update"]["sessionUpdate"]
            .as_str()
            .map(str::to_owned)
    }

    /// Everything except the pass-through ack of the inbound notification.
    fn renderable(out: &[AcpClientMessage]) -> Vec<&AcpClientMessage> {
        out.iter()
            .filter(|message| update_method(message).as_deref() != Some(METHOD))
            .collect()
    }

    fn child_session_of(message: &AcpClientMessage) -> Option<String> {
        match message {
            AcpClientMessage::SessionNotification(args) => {
                Some(args.request.session_id.0.as_ref().to_owned())
            }
            _ => None,
        }
    }

    #[test]
    fn created_status_announces_the_subagent_on_the_root_session() {
        let mut registry = ChildRegistry::default();
        let out = expand(
            &mut registry,
            json!({
                "rootSessionId": "ses_root",
                "childSessionId": "ses_child",
                "parentSessionId": "ses_root",
                "depth": 1,
                "title": "scan src/",
                "type": "status",
                "status": "created",
            }),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            update_method(&out[0]).as_deref(),
            Some(SESSION_NOTIFICATION)
        );
        assert_eq!(
            session_update_name(&out[0]).as_deref(),
            Some("subagent_spawned")
        );
    }

    #[test]
    fn spawn_is_announced_exactly_once_across_updates() {
        let mut registry = ChildRegistry::default();
        let created = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "status",
            "status": "created",
        });
        assert_eq!(expand(&mut registry, created.clone()).len(), 1);

        let running = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "status",
            "status": "running",
        });
        assert!(
            renderable(&expand(&mut registry, running)).is_empty(),
            "a running status re-announces nothing"
        );

        let update = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "update",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hi" }
            },
        });
        let out = expand(&mut registry, update);
        assert_eq!(out.len(), 1, "only the child's own update is forwarded");
        assert_eq!(child_session_of(&out[0]).as_deref(), Some("ses_child"));
    }

    #[test]
    fn update_before_status_still_spawns_first() {
        let mut registry = ChildRegistry::default();
        let update = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "depth": 1,
            "title": "explore",
            "type": "update",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking" }
            },
        });
        let out = expand(&mut registry, update);
        assert_eq!(out.len(), 2, "spawn must precede the first child update");
        assert_eq!(
            session_update_name(&out[0]).as_deref(),
            Some("subagent_spawned")
        );
        assert_eq!(child_session_of(&out[1]).as_deref(), Some("ses_child"));
    }

    #[test]
    fn terminal_status_finishes_the_subagent_once() {
        let mut registry = ChildRegistry::default();
        let created = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "status",
            "status": "created",
        });
        expand(&mut registry, created);

        let done = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "status",
            "status": "failed",
            "error": { "type": "tool_error", "message": "boom" },
        });
        let out = expand(&mut registry, done.clone());
        assert_eq!(out.len(), 1);
        assert_eq!(
            session_update_name(&out[0]).as_deref(),
            Some("subagent_finished")
        );

        assert!(
            renderable(&expand(&mut registry, done)).is_empty(),
            "finish is latched"
        );
    }

    #[test]
    fn nested_child_is_announced_under_the_root_session() {
        let mut registry = ChildRegistry::default();
        let out = expand(
            &mut registry,
            json!({
                "rootSessionId": "ses_root",
                "childSessionId": "ses_grandchild",
                "parentSessionId": "ses_child",
                "depth": 2,
                "title": "nested",
                "type": "status",
                "status": "created",
            }),
        );
        assert_eq!(out.len(), 1);
        let AcpClientMessage::ExtNotification(args) = &out[0] else {
            panic!("expected a session notification");
        };
        let payload: Value = serde_json::from_str(args.request.params.get()).expect("json");
        assert_eq!(payload["sessionId"], "ses_root");
        assert_eq!(payload["update"]["parent_session_id"], "ses_child");
    }

    #[test]
    fn malformed_payload_is_acked_but_not_forwarded() {
        let mut registry = ChildRegistry::default();
        let out = registry.expand(ext(json!("not an object")));
        assert_eq!(out.len(), 1, "the notification still needs its ack");
        assert_eq!(update_method(&out[0]).as_deref(), Some(METHOD));
    }

    #[test]
    fn clearing_the_registry_re_announces_after_a_reconnect() {
        let mut registry = ChildRegistry::default();
        let created = json!({
            "rootSessionId": "ses_root",
            "childSessionId": "ses_child",
            "parentSessionId": "ses_root",
            "type": "status",
            "status": "created",
        });
        expand(&mut registry, created.clone());
        registry.clear();
        assert_eq!(expand(&mut registry, created).len(), 1);
    }

    #[test]
    fn recognizes_only_its_own_method() {
        let args = ext(json!({}));
        assert!(ChildRegistry::handles(&AcpClientMessage::ExtNotification(
            args
        )));
        let other = acp::ExtNotification::new(
            "x.ai/session_notification",
            serde_json::value::to_raw_value(&json!({})).unwrap().into(),
        );
        let (tx, _rx) = oneshot::channel();
        assert!(!ChildRegistry::handles(&AcpClientMessage::ExtNotification(
            AcpArgs {
                request: other,
                response_tx: tx
            }
        )));
    }

    #[test]
    fn child_meta_extracts_the_kernel_stamp() {
        let payload = json!({
            "opencode/child-session": { "id": "ses_child", "parentID": "ses_root", "depth": 1 }
        });
        let (id, parent) = child_meta(payload.as_object().expect("object")).expect("meta");
        assert_eq!(id, "ses_child");
        assert_eq!(parent, "ses_root");
    }
}
