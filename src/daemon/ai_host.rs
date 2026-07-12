//! Daemon owner-lane host for DJ Gem chat and its retained `ai` projection.

use crate::ai::{AiEvent, AiHandle, GeminiModel};
use crate::api::Song;
use crate::remote::proto::{AiMessageModel, AiRoleModel, RemoteCommand, RemoteResponse};
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;

use super::engine::{DaemonEngine, EngineEffect};
use super::events::{DaemonEvent, DaemonEventSender, record_daemon_event};

pub(super) struct AiHost {
    handle: Option<AiHandle>,
    /// The credentials the live actor was spawned with. Compared against the current
    /// config on every ask: a removed/rotated key drops the actor (a revoked key must
    /// never be reused — the TUI's ReloadAi equivalent), a model-only change retargets
    /// the live actor via `set_model`.
    spawned: Option<(String, GeminiModel)>,
    event_tx: DaemonEventSender,
    projection: AiProjection,
}

pub(super) struct Intercepted {
    pub(super) event: Option<DaemonEvent>,
    pub(super) effects: Vec<EngineEffect>,
}

impl AiHost {
    pub(super) fn new(event_tx: DaemonEventSender) -> Self {
        Self {
            handle: None,
            spawned: None,
            event_tx,
            projection: AiProjection::default(),
        }
    }

    pub(super) fn publish(&self, engine: &DaemonEngine, publisher: &mut Publisher) {
        let suggestions = self
            .projection
            .suggestions
            .iter()
            .map(|song| {
                crate::remote::publish::track_model(song, engine.library(), engine.signals())
            })
            .collect();
        publisher.publish_ai(
            self.projection.messages.clone(),
            self.projection.thinking,
            suggestions,
        );
    }

    /// Intercept AI commands and actor events before ordinary engine dispatch.
    pub(super) async fn intercept(
        &mut self,
        event: DaemonEvent,
        engine: &mut DaemonEngine,
        publisher: &mut Publisher,
    ) -> Intercepted {
        match event {
            DaemonEvent::Ai(event) => {
                let effects = self.on_event(event, engine, publisher).await;
                Intercepted {
                    event: None,
                    effects,
                }
            }
            DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => {
                match self.command(command, engine, publisher) {
                    Ok(response) => {
                        let _ = reply.send(response);
                        Intercepted {
                            event: None,
                            effects: Vec::new(),
                        }
                    }
                    Err(command) => Intercepted {
                        event: Some(DaemonEvent::Remote(RemoteEvent::Command(command, reply))),
                        effects: Vec::new(),
                    },
                }
            }
            DaemonEvent::Remote(RemoteEvent::SessionCommand {
                command,
                origin,
                reply,
            }) => match self.command(command, engine, publisher) {
                Ok(response) => {
                    let _ = reply.send(response);
                    Intercepted {
                        event: None,
                        effects: Vec::new(),
                    }
                }
                Err(command) => Intercepted {
                    event: Some(DaemonEvent::Remote(RemoteEvent::SessionCommand {
                        command,
                        origin,
                        reply,
                    })),
                    effects: Vec::new(),
                },
            },
            event => Intercepted {
                event: Some(event),
                effects: Vec::new(),
            },
        }
    }

    fn command(
        &mut self,
        command: RemoteCommand,
        engine: &DaemonEngine,
        publisher: &mut Publisher,
    ) -> Result<RemoteResponse, RemoteCommand> {
        let RemoteCommand::AskAi { ticket: _, prompt } = command else {
            return Err(command);
        };
        let (key, model) = engine.ai_runtime_config();
        if let Err(reason) = self.ensure_handle(key.as_deref(), model) {
            return Ok(RemoteResponse::err(reason));
        }
        self.projection.push(AiRoleModel::User, prompt.clone());
        self.projection.thinking = true;
        self.publish(engine, publisher);
        if self
            .handle
            .as_ref()
            .expect("AI handle was ensured above")
            .ask(prompt, Box::new(engine.build_ai_context()))
            .is_err()
        {
            // The ask never entered the actor: pop the dangling user bubble so the
            // retained transcript doesn't show a question that will never be answered.
            self.projection.messages.pop();
            self.projection.thinking = false;
            self.publish(engine, publisher);
            return Ok(RemoteResponse::err("busy"));
        }
        Ok(RemoteResponse::ok("asking".to_owned()))
    }

    fn ensure_handle(
        &mut self,
        key: Option<&str>,
        model: GeminiModel,
    ) -> Result<&AiHandle, &'static str> {
        let key = key.ok_or_else(|| {
            // Key removed (set_gemini_key "", reset_all_settings): retire the live
            // actor so the revoked credential is never used again.
            self.handle = None;
            self.spawned = None;
            "ai_disabled"
        })?;
        match &self.spawned {
            Some((spawned_key, _)) if spawned_key != key => {
                // Rotated key: the old actor still holds the revoked credential.
                self.handle = None;
                self.spawned = None;
            }
            Some((_, spawned_model)) if *spawned_model != model && self.handle.is_some() => {
                if let Some(handle) = &self.handle {
                    let _ = handle.set_model(model);
                }
                self.spawned = Some((key.to_owned(), model));
            }
            _ => {}
        }
        if self.handle.is_none() {
            let event_tx = self.event_tx.clone();
            self.handle = crate::ai::spawn(key, model, move |event| {
                record_daemon_event(&event_tx, DaemonEvent::Ai(event));
            });
            self.spawned = self.handle.as_ref().map(|_| (key.to_owned(), model));
        }
        self.handle.as_ref().ok_or("ai_disabled")
    }

    async fn on_event(
        &mut self,
        event: AiEvent,
        engine: &mut DaemonEngine,
        publisher: &mut Publisher,
    ) -> Vec<EngineEffect> {
        let mut publish = false;
        let effects = match event {
            AiEvent::Thinking(thinking) => {
                self.projection.thinking = thinking;
                publish = true;
                Vec::new()
            }
            AiEvent::Chat(text) => {
                self.projection.push(AiRoleModel::Assistant, text);
                self.projection.thinking = false;
                publish = true;
                Vec::new()
            }
            AiEvent::Error(text) => {
                self.projection.push(AiRoleModel::Assistant, text);
                self.projection.thinking = false;
                publish = true;
                Vec::new()
            }
            AiEvent::Suggestions(songs) => {
                self.projection.suggestions = songs;
                publish = true;
                Vec::new()
            }
            AiEvent::PlayTracks(songs) => {
                engine.ai_play_tracks(songs).await;
                Vec::new()
            }
            AiEvent::Enqueue(songs) => {
                engine.ai_enqueue(songs).await;
                Vec::new()
            }
            AiEvent::SetAutoplay(on) => engine.ai_set_autoplay(on),
            AiEvent::CreatePlaylist(name) => {
                engine.ai_create_playlist(&name);
                Vec::new()
            }
            AiEvent::AddToPlaylist { playlist, songs } => {
                engine.ai_add_to_playlist(&playlist, songs);
                Vec::new()
            }
            AiEvent::PlayPlaylist(name) => {
                engine.ai_play_playlist(&name).await;
                Vec::new()
            }
            AiEvent::StreamingPicks { .. }
            | AiEvent::StationPatch { .. }
            | AiEvent::RomanizedTitles { .. }
            | AiEvent::SetStationProfile { .. } => {
                tracing::debug!("ignored non-chat AI event in daemon AI host");
                Vec::new()
            }
        };
        if publish {
            self.publish(engine, publisher);
        }
        effects
    }
}

/// Mirrors the TUI transcript cap (`AI_HISTORY_MAX`) while retaining full topic state.
const AI_HISTORY_MAX: usize = 999;

#[derive(Default)]
struct AiProjection {
    messages: Vec<AiMessageModel>,
    thinking: bool,
    suggestions: Vec<Song>,
}

impl AiProjection {
    fn push(&mut self, role: AiRoleModel, text: String) {
        self.messages.push(AiMessageModel { role, text });
        if self.messages.len() > AI_HISTORY_MAX {
            let overflow = self.messages.len() - AI_HISTORY_MAX;
            self.messages.drain(0..overflow);
        }
    }

    #[cfg(test)]
    fn reduce_chat(&mut self, event: AiEvent) {
        match event {
            AiEvent::Thinking(thinking) => self.thinking = thinking,
            AiEvent::Chat(text) | AiEvent::Error(text) => {
                self.push(AiRoleModel::Assistant, text);
                self.thinking = false;
            }
            AiEvent::Suggestions(songs) => self.suggestions = songs,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("title-{id}"), "artist", "3:00")
    }

    #[test]
    fn ask_thinking_chat_and_error_update_the_retained_transcript() {
        let mut state = AiProjection::default();
        state.push(AiRoleModel::User, "hello".to_owned());
        state.reduce_chat(AiEvent::Thinking(true));
        assert!(state.thinking);
        state.reduce_chat(AiEvent::Chat("hi".to_owned()));
        assert!(!state.thinking);
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1].role, AiRoleModel::Assistant);
        assert_eq!(state.messages[1].text, "hi");

        state.reduce_chat(AiEvent::Thinking(true));
        state.reduce_chat(AiEvent::Error("failed".to_owned()));
        assert!(!state.thinking);
        assert_eq!(state.messages.last().unwrap().text, "failed");
    }

    #[test]
    fn suggestions_replace_the_retained_list() {
        let mut state = AiProjection::default();
        state.reduce_chat(AiEvent::Suggestions(vec![song("one"), song("two")]));
        assert_eq!(state.suggestions.len(), 2);
        state.reduce_chat(AiEvent::Suggestions(vec![song("new")]));
        assert_eq!(state.suggestions[0].video_id, "new");
    }

    #[test]
    fn transcript_evicts_oldest_messages_at_the_tui_cap() {
        let mut state = AiProjection::default();
        for index in 0..=AI_HISTORY_MAX {
            state.push(AiRoleModel::User, index.to_string());
        }
        assert_eq!(state.messages.len(), AI_HISTORY_MAX);
        assert_eq!(state.messages[0].text, "1");
    }

    #[tokio::test]
    async fn missing_key_is_ai_disabled_without_spawning_the_actor() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut host = AiHost::new(DaemonEventSender::new(tx));
        assert!(matches!(
            host.ensure_handle(None, GeminiModel::default()),
            Err("ai_disabled")
        ));
        assert!(host.handle.is_none());
    }
}
