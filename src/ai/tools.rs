//! The 13 assistant tools: their Gemini `functionDeclarations` and an async dispatcher.
//!
//! Ported from `youtube-music-cli`'s tool layer, adapted to TEA: a tool can't mutate
//! `App`, so a *mutation* tool emits a [`Msg`] (applied by `update()`) and reports the
//! intended outcome in its `functionResponse`; a *read* tool answers from the
//! [`AiContext`] snapshot; *search/resolve* tools shell out to yt-dlp (the same backend
//! anonymous search uses) and cache `videoId → Song` so later tools can act on bare ids.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::Song;
use crate::api::ytmusic::{related_tracks_from_source, ytdlp_search};
use crate::app::{AiContext, Msg};

/// Default number of tracks a query resolves to when the model doesn't ask for a count.
const DEFAULT_RESOLVE: usize = 1;
/// How many related tracks streaming / suggestions pull in.
const RELATED_COUNT: usize = 8;
/// Per-conversation tool-result cache cap.
const TOOL_CACHE_MAX: usize = 999;

/// What a tool call needs to do its work.
pub struct ToolDeps<'a> {
    pub ctx: &'a AiContext,
    /// `videoId → Song`, populated by searches so later tools resolve bare ids.
    pub cache: &'a mut HashMap<String, Song>,
    pub msg_tx: &'a UnboundedSender<Msg>,
    /// Set true once any playback/queue/playlist mutation has run (disables model fallback).
    pub side_effected: &'a mut bool,
}

/// The full set of tool schemas to advertise to Gemini.
pub fn declarations() -> Vec<Value> {
    vec![
        decl(
            "search_tracks",
            "Search YouTube for tracks matching a query. Returns a list with videoId, title, and artist. Use this to find tracks before playing or queueing them.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for, e.g. an artist, song, or genre." },
                    "limit": { "type": "integer", "description": "Max results (1-20). Defaults to 10." }
                },
                "required": ["query"]
            }),
        ),
        decl(
            "play_music",
            "Immediately start playing music. Provide a natural-language query (the best match plays) or a videoId from a prior search.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to play, e.g. 'lo-fi beats' or a song title." },
                    "videoId": { "type": "string", "description": "A specific videoId to play (from search_tracks)." }
                },
                "required": []
            }),
        ),
        decl(
            "add_to_queue",
            "Append one or more tracks to the play queue. Provide a query, a single videoId, or a list of videoIds.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to queue." },
                    "limit": { "type": "integer", "description": "How many results to queue when using a query (1-20). Defaults to 5." },
                    "videoId": { "type": "string", "description": "A single videoId to queue." },
                    "videoIds": { "type": "array", "items": { "type": "string" }, "description": "Several videoIds to queue." }
                },
                "required": []
            }),
        ),
        decl(
            "get_queue",
            "Get the current track and the upcoming queue.",
            json!({ "type": "object", "properties": {} }),
        ),
        decl(
            "start_streaming",
            "Start endless streaming: queue tracks related to a seed (defaults to the current track) and turn on autoplay so the queue keeps refilling. When the user describes a *vibe* (e.g. 'chill late-night drive, nothing too poppy'), also set `explore` to how adventurous the station should be and `avoid_artists` for anyone they want kept out — these shape every future refill, not just the first batch.",
            json!({
                "type": "object",
                "properties": {
                    "seed": { "type": "string", "description": "Seed to base streaming on (an artist, song, or vibe). Defaults to what's playing." },
                    "explore": {
                        "type": "string",
                        "enum": ["tight", "balanced", "wide"],
                        "description": "How adventurous the station should be: 'tight' = stay close to the seed, 'balanced' = the default mix, 'wide' = lots of discovery. Omit unless the vibe implies one."
                    },
                    "avoid_artists": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Artist names to keep out of the station, if the user asked to avoid them."
                    }
                },
                "required": []
            }),
        ),
        decl(
            "stop_streaming",
            "Turn off autoplay streaming (the queue stops auto-refilling).",
            json!({ "type": "object", "properties": {} }),
        ),
        decl(
            "get_suggestions",
            "Get a list of tracks related to a seed (defaults to the current track) and show them as pickable suggestions. Does not change playback.",
            json!({
                "type": "object",
                "properties": {
                    "seed": { "type": "string", "description": "Seed to base suggestions on. Defaults to what's playing." }
                },
                "required": []
            }),
        ),
        decl(
            "get_user_playlists",
            "List the user's local playlists with their track counts.",
            json!({ "type": "object", "properties": {} }),
        ),
        decl(
            "play_playlist",
            "Play one of the user's local playlists by name or id.",
            json!({
                "type": "object",
                "properties": { "playlist": { "type": "string", "description": "Playlist name or id." } },
                "required": ["playlist"]
            }),
        ),
        decl(
            "create_playlist",
            "Create a new empty local playlist.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Name for the new playlist." } },
                "required": ["name"]
            }),
        ),
        decl(
            "add_to_playlist",
            "Add tracks to a local playlist. Provide the playlist name/id and a query, videoId, or list of videoIds.",
            json!({
                "type": "object",
                "properties": {
                    "playlist": { "type": "string", "description": "Target playlist name or id." },
                    "query": { "type": "string", "description": "What to add." },
                    "videoId": { "type": "string" },
                    "videoIds": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["playlist"]
            }),
        ),
        decl(
            "get_track_info",
            "Get details (title, artist, duration) for a track by videoId or query.",
            json!({
                "type": "object",
                "properties": {
                    "videoId": { "type": "string" },
                    "query": { "type": "string" }
                },
                "required": []
            }),
        ),
        decl(
            "get_user_favorites",
            "List the user's favorited tracks.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

fn decl(name: &str, description: &str, parameters: Value) -> Value {
    json!({ "name": name, "description": description, "parameters": parameters })
}

/// Execute one tool call, returning the JSON `result` to report back to the model.
pub async fn execute_tool(name: &str, args: &Value, deps: &mut ToolDeps<'_>) -> Value {
    match name {
        "search_tracks" => {
            let Some(query) = str_arg(args, "query") else {
                return err("search_tracks requires a 'query'");
            };
            let limit = uint_arg(args, "limit").unwrap_or(10).clamp(1, 20);
            match ytdlp_search(&query, limit).await {
                Ok(songs) => {
                    cache_all(deps, &songs);
                    json!({ "results": songs.iter().map(track_json).collect::<Vec<_>>() })
                }
                Err(e) => err(&format!("search failed: {e}")),
            }
        }

        "play_music" => {
            let songs = resolve_songs(args, deps, DEFAULT_RESOLVE).await;
            let Some(first) = songs.into_iter().next() else {
                return err("nothing matched — try search_tracks first");
            };
            let label = fmt_song(&first);
            send(deps, Msg::AiPlayTracks(vec![first]));
            json!({ "playing": label })
        }

        "add_to_queue" => {
            let songs = resolve_songs(args, deps, 5).await;
            let count = songs.len();
            let labels: Vec<String> = songs.iter().map(fmt_song).collect();
            send(deps, Msg::AiEnqueue(songs));
            json!({ "queued": count, "tracks": labels })
        }

        "get_queue" => json!({
            "current": deps.ctx.current_track,
            "radioStation": deps.ctx.current_radio_station,
            "radioNowPlaying": deps.ctx.current_radio_now_playing,
            "upcoming": deps.ctx.queue_upcoming,
            "length": deps.ctx.queue_len,
            "remaining": deps.ctx.queue_remaining,
        }),

        "start_streaming" => {
            let seed = str_arg(args, "seed")
                .or_else(|| deps.ctx.current_track.clone())
                .unwrap_or_else(|| "popular music".to_owned());
            let songs = tool_related_tracks(&seed, RELATED_COUNT, deps.ctx).await;
            cache_all(deps, &songs);
            let count = songs.len();
            // A vibe-shaped station carries an explore level and/or artists to avoid → persist it
            // as a profile the engine applies to every refill. A plain "start streaming" (no shaping
            // hints) leaves any existing station untouched.
            let explore = str_arg(args, "explore");
            let avoid = str_list_arg(args, "avoid_artists");
            if explore.is_some() || !avoid.is_empty() {
                send(
                    deps,
                    Msg::AiSetStationProfile {
                        query: seed.clone(),
                        explore: explore.clone(),
                        avoid_artists: avoid,
                    },
                );
            }
            send(deps, Msg::AiEnqueue(songs));
            send(deps, Msg::AiSetAutoplay(true));
            json!({ "started": true, "seed": seed, "queued": count, "explore": explore })
        }

        "stop_streaming" => {
            send(deps, Msg::AiSetAutoplay(false));
            json!({ "stopped": true })
        }

        "get_suggestions" => {
            let seed = str_arg(args, "seed")
                .or_else(|| deps.ctx.current_track.clone())
                .unwrap_or_else(|| "popular music".to_owned());
            let songs = tool_related_tracks(&seed, RELATED_COUNT, deps.ctx).await;
            cache_all(deps, &songs);
            let labels: Vec<String> = songs.iter().map(fmt_song).collect();
            // Populating the pickable list is not a playback mutation → no side-effect flag.
            let _ = deps.msg_tx.send(Msg::AiSuggestions(songs));
            json!({ "suggestions": labels })
        }

        "get_user_playlists" => json!({
            "playlists": deps.ctx.playlists.iter()
                .map(|p| json!({ "id": p.id, "name": p.name, "tracks": p.count }))
                .collect::<Vec<_>>()
        }),

        "play_playlist" => {
            let Some(playlist) = str_arg(args, "playlist") else {
                return err("play_playlist requires a 'playlist'");
            };
            send(deps, Msg::AiPlayPlaylist(playlist.clone()));
            json!({ "playing_playlist": playlist })
        }

        "create_playlist" => {
            let Some(name) = str_arg(args, "name") else {
                return err("create_playlist requires a 'name'");
            };
            send(deps, Msg::AiCreatePlaylist(name.clone()));
            json!({ "created": name })
        }

        "add_to_playlist" => {
            let Some(playlist) = str_arg(args, "playlist") else {
                return err("add_to_playlist requires a 'playlist'");
            };
            let songs = resolve_songs(args, deps, 5).await;
            if songs.is_empty() {
                return err("nothing matched to add — try search_tracks first");
            }
            let count = songs.len();
            send(
                deps,
                Msg::AiAddToPlaylist {
                    playlist: playlist.clone(),
                    songs,
                },
            );
            json!({ "added": count, "playlist": playlist })
        }

        "get_track_info" => {
            let songs = resolve_songs(args, deps, 1).await;
            match songs.into_iter().next() {
                Some(s) => track_json(&s),
                None => err("no track matched"),
            }
        }

        "get_user_favorites" => json!({ "favorites": deps.ctx.favorites }),

        other => err(&format!("unknown tool: {other}")),
    }
}

async fn tool_related_tracks(seed: &str, limit: usize, ctx: &AiContext) -> Vec<Song> {
    let config = ctx.search.clone().normalized();
    let source = config.normalized_streaming_source(config.streaming_source);
    let selected_sources = if source == crate::search_source::SearchSource::All {
        config.streaming_enabled_sources()
    } else {
        vec![source]
    };
    let per_source_limit = if source == crate::search_source::SearchSource::All {
        (limit / selected_sources.len().max(1)).max(4).min(limit)
    } else {
        limit
    };
    let mut out = Vec::with_capacity(limit);
    let mut emitted_ids = HashSet::new();
    for source in selected_sources {
        if out.len() >= limit {
            break;
        }
        let source_limit = if config.streaming_source == crate::search_source::SearchSource::All {
            per_source_limit
        } else {
            limit.saturating_sub(out.len())
        };
        let Ok(songs) = related_tracks_from_source(
            seed,
            source,
            &config,
            source_limit,
            &emitted_ids,
            crate::streaming::StreamingMode::Balanced,
        )
        .await
        else {
            continue;
        };
        for song in songs {
            if emitted_ids.insert(song.video_id.clone()) {
                out.push(song);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out
}

/// Resolve a tool's track argument(s) to concrete songs: a `videoIds` list, a single
/// `videoId`, or a `query` (searched). Bare ids not in the cache become minimal songs so
/// playback still works when the model supplies an id from its own knowledge.
async fn resolve_songs(args: &Value, deps: &mut ToolDeps<'_>, default_limit: usize) -> Vec<Song> {
    if let Some(ids) = args.get("videoIds").and_then(Value::as_array) {
        let out: Vec<Song> = ids
            .iter()
            .filter_map(Value::as_str)
            .map(|id| deps.cache.get(id).cloned().unwrap_or_else(|| bare_song(id)))
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(id) = str_arg(args, "videoId") {
        return vec![
            deps.cache
                .get(&id)
                .cloned()
                .unwrap_or_else(|| bare_song(&id)),
        ];
    }
    if let Some(query) = str_arg(args, "query") {
        let limit = uint_arg(args, "limit")
            .unwrap_or(default_limit)
            .clamp(1, 20);
        if let Ok(songs) = ytdlp_search(&query, limit).await {
            cache_all(deps, &songs);
            return songs;
        }
    }
    Vec::new()
}

fn send(deps: &mut ToolDeps<'_>, msg: Msg) {
    *deps.side_effected = true;
    let _ = deps.msg_tx.send(msg);
}

fn cache_all(deps: &mut ToolDeps<'_>, songs: &[Song]) {
    if deps.cache.len().saturating_add(songs.len()) > TOOL_CACHE_MAX {
        deps.cache.clear();
    }
    for s in songs {
        deps.cache.insert(s.video_id.clone(), s.clone());
    }
}

fn bare_song(id: &str) -> Song {
    Song::remote(id, id, "", "")
}

fn fmt_song(s: &Song) -> String {
    if s.artist.is_empty() {
        s.title.clone()
    } else {
        format!("{} — {}", s.title, s.artist)
    }
}

fn track_json(s: &Song) -> Value {
    json!({ "videoId": s.video_id, "title": s.title, "artist": s.artist, "duration": s.duration })
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn uint_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

/// Read a string-array argument, trimming and dropping blanks (empty when absent or not an array).
fn str_list_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn err(msg: &str) -> Value {
    json!({ "error": msg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_list_arg_trims_drops_blanks_and_non_strings() {
        let args = json!({ "avoid_artists": ["  A  ", "", "B", 7, "  "] });
        assert_eq!(
            str_list_arg(&args, "avoid_artists"),
            vec!["A".to_owned(), "B".to_owned()]
        );
        assert!(str_list_arg(&args, "missing").is_empty());
        assert!(
            str_list_arg(&json!({ "avoid_artists": "notanarray" }), "avoid_artists").is_empty()
        );
    }

    #[test]
    fn start_streaming_advertises_vibe_shaping_params() {
        let decls = declarations();
        let sr = decls
            .iter()
            .find(|d| d["name"] == "start_streaming")
            .expect("start_streaming declared");
        let props = &sr["parameters"]["properties"];
        assert!(props.get("explore").is_some(), "explore param advertised");
        assert!(
            props.get("avoid_artists").is_some(),
            "avoid_artists param advertised"
        );
        assert_eq!(
            props["explore"]["enum"][0], "tight",
            "explore is a constrained enum"
        );
    }

    #[test]
    fn all_thirteen_tools_declared() {
        let decls = declarations();
        assert_eq!(decls.len(), 13);
        // Every declaration has the required shape.
        for d in &decls {
            assert!(d.get("name").and_then(Value::as_str).is_some());
            assert!(d.get("description").is_some());
            assert_eq!(d["parameters"]["type"], "object");
        }
        let names: Vec<&str> = decls.iter().filter_map(|d| d["name"].as_str()).collect();
        for expected in [
            "search_tracks",
            "play_music",
            "add_to_queue",
            "get_queue",
            "start_streaming",
            "stop_streaming",
            "get_suggestions",
            "get_user_playlists",
            "play_playlist",
            "create_playlist",
            "add_to_playlist",
            "get_track_info",
            "get_user_favorites",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    fn ctx() -> AiContext {
        AiContext {
            current_track: Some("Now — Artist".to_owned()),
            current_radio_station: None,
            current_radio_now_playing: None,
            queue_upcoming: vec!["Next — Artist".to_owned()],
            queue_len: 2,
            queue_remaining: 1,
            recent_history: vec![],
            favorites: vec!["Fave — Artist".to_owned()],
            playlists: vec![],
            search: crate::search_source::SearchConfig::default(),
            authenticated: false,
            autoplay_streaming: false,
        }
    }

    #[tokio::test]
    async fn read_tools_answer_from_context_without_side_effects() {
        let ctx = ctx();
        let mut cache = HashMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut side = false;
        let mut deps = ToolDeps {
            ctx: &ctx,
            cache: &mut cache,
            msg_tx: &tx,
            side_effected: &mut side,
        };

        let q = execute_tool("get_queue", &json!({}), &mut deps).await;
        assert_eq!(q["current"], "Now — Artist");
        assert_eq!(q["remaining"], 1);

        let f = execute_tool("get_user_favorites", &json!({}), &mut deps).await;
        assert_eq!(f["favorites"][0], "Fave — Artist");

        assert!(!side, "read tools must not set the side-effect flag");
    }

    #[tokio::test]
    async fn get_queue_reports_radio_stream_now_playing() {
        let mut ctx = ctx();
        ctx.current_track = Some("Groove Radio — US / MP3 / 128k".to_owned());
        ctx.current_radio_station = ctx.current_track.clone();
        ctx.current_radio_now_playing = Some("Track — Artist".to_owned());
        let mut cache = HashMap::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut side = false;
        let mut deps = ToolDeps {
            ctx: &ctx,
            cache: &mut cache,
            msg_tx: &tx,
            side_effected: &mut side,
        };

        let q = execute_tool("get_queue", &json!({}), &mut deps).await;

        assert_eq!(q["radioStation"], "Groove Radio — US / MP3 / 128k");
        assert_eq!(q["radioNowPlaying"], "Track — Artist");
        assert!(!side, "read tools must not set the side-effect flag");
    }

    #[tokio::test]
    async fn play_music_by_cached_video_id_emits_intent() {
        let ctx = ctx();
        let mut cache = HashMap::new();
        cache.insert("vid1".to_owned(), Song::remote("vid1", "T", "A", "1:00"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut side = false;
        let mut deps = ToolDeps {
            ctx: &ctx,
            cache: &mut cache,
            msg_tx: &tx,
            side_effected: &mut side,
        };

        let r = execute_tool("play_music", &json!({ "videoId": "vid1" }), &mut deps).await;
        assert_eq!(r["playing"], "T — A");
        assert!(side, "play_music is a mutation");
        match rx.try_recv().unwrap() {
            Msg::AiPlayTracks(songs) => assert_eq!(songs[0].video_id, "vid1"),
            _ => panic!("expected AiPlayTracks"),
        }
    }

    #[tokio::test]
    async fn stop_streaming_emits_autoplay_off() {
        let ctx = ctx();
        let mut cache = HashMap::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut side = false;
        let mut deps = ToolDeps {
            ctx: &ctx,
            cache: &mut cache,
            msg_tx: &tx,
            side_effected: &mut side,
        };
        execute_tool("stop_streaming", &json!({}), &mut deps).await;
        assert!(matches!(rx.try_recv().unwrap(), Msg::AiSetAutoplay(false)));
    }
}
