// DJ Gem chat (docs/gui/07 §12): a ticketed ask over the `ai` topic. The user bubble is
// added optimistically; the core echoes the full transcript (plus the thinking flag and
// playable suggestions) on its push, so the store always mirrors authoritative state. The
// desktop bridge forwards ask_ai once the core advertises the `ai` capability; until then
// the in-page demo core answers (see gui/WIRING.md).

import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

export interface AiMessage {
  role: 'user' | 'assistant';
  text: string;
}

// PROVISIONAL `ai` topic shape — only the demo core speaks it. Reconcile with the M4 core
// wire + ts-rs types when they land (mirrors the search/library provisional note).
export interface AiState {
  kind: 'ai_state';
  messages: AiMessage[];
  thinking: boolean;
  suggestions: TrackModel[];
}

export class AiStore {
  messages = $state<AiMessage[]>([]);
  thinking = $state(false);
  suggestions = $state<TrackModel[]>([]);
  #ticket = 0;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('ai', (payload) => this.#onPush(payload as AiState));
  }

  get started(): boolean {
    return this.messages.length > 0;
  }

  ask(prompt: string): void {
    const text = prompt.trim();
    if (!text) return;
    this.#ticket += 1;
    // Optimistic user bubble; the core's push replaces the transcript wholesale.
    this.messages = [...this.messages, { role: 'user', text }];
    this.thinking = true;
    this.suggestions = [];
    this.#client.cmd('ask_ai', { ticket: this.#ticket, prompt: text });
  }

  /** Play a suggested track. */
  play(track: TrackModel): void {
    this.#client.cmd('play_tracks', { video_ids: [track.video_id] });
  }

  #onPush(s: AiState): void {
    if (s.kind !== 'ai_state') return;
    this.messages = s.messages;
    this.thinking = s.thinking;
    this.suggestions = s.suggestions;
  }
}
