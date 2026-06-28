//! Grouped sub-states of [`App`] (Stage 2 of the re-architecture).
//!
//! The reducer historically kept ~70 flat fields on `App`; these structs gather the
//! cohesive per-domain groups so ownership reads clearly and future changes stay local.
//! Behaviour-preserving: the fields are the same, just nested (`app.search.input`).

use super::*;

/// Search-screen state: the query, its results, selection, focus, and in-flight flag.
#[derive(Default)]
pub struct SearchState {
    /// The search query being typed.
    pub input: String,
    /// Whether Ctrl+A has selected the whole query (desktop-style: the next edit
    /// replaces or clears it). Reset on any consuming keypress.
    pub select_all: bool,
    /// Whether the input box or the results list has focus.
    pub focus: SearchFocus,
    /// The current search results.
    pub results: Vec<Song>,
    /// The highlighted result row.
    pub selected: usize,
    /// True between issuing a search request and its results arriving.
    pub searching: bool,
}

/// AI-assistant state: availability, model, the chat transcript, the prompt being
/// typed, and the pickable suggestions list with its focus.
#[derive(Default)]
pub struct AiState {
    /// Whether a Gemini API key is configured (gates the assistant; `false` → onboarding).
    pub available: bool,
    /// The Gemini model the assistant uses (shown in the AI view header).
    pub model: GeminiModel,
    /// The chat transcript (user prompts, assistant replies, errors).
    pub messages: Vec<AiMessage>,
    /// The AI prompt being typed.
    pub input: String,
    /// Whether Ctrl+A has selected the whole AI prompt (next edit replaces/clears it).
    pub select_all: bool,
    /// True while a request is in flight (drives the spinner; blocks a second request).
    pub thinking: bool,
    /// The pickable related-tracks list (get_suggestions).
    pub suggestions: Vec<Song>,
    /// The highlighted suggestion row.
    pub suggestions_selected: usize,
    /// Whether the input box or the suggestions list has focus.
    pub focus: AiFocus,
}
