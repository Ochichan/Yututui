// Accounts (docs/gui/07 §11, docs/gui/02 §13.4): Last.fm / ListenBrainz / Spotify connection
// state + scrobble toggles. Connect flows are browser-approval: the core mints an auth URL
// and pushes it on the `accounts` topic; the GUI opens it via the native win:openUrl op, and
// the same topic later pushes the connected snapshot. Secrets (tokens, client secrets) are
// write-only — only presence/connected flags round-trip. The desktop bridge forwards these
// once the core advertises `accounts-v8`; until then the in-page demo core answers (see
// gui/WIRING.md).

import type { Client } from '../ipc/client';

// PROVISIONAL wire shapes — only the demo core speaks them. Reconcile with the M4 core wire +
// ts-rs types when they land.
export interface LastfmAccount {
  connected: boolean;
  user: string | null;
  scrobbling: boolean;
  love_sync: boolean;
}
export interface ListenBrainzAccount {
  submit: boolean;
  /** Presence only — the token never round-trips. */
  has_token: boolean;
  custom_url: string | null;
}
export interface SpotifyAccount {
  connected: boolean;
  user: string | null;
  client_id: string | null;
  redirect_port: number | null;
}
export interface AccountsSnapshot {
  kind: 'accounts_snapshot';
  lastfm: LastfmAccount;
  listenbrainz: ListenBrainzAccount;
  spotify: SpotifyAccount;
  scrobble_local: boolean;
}
/** Same topic, connect half: the browser-approval URL to open. */
export interface AccountsAuthUrl {
  kind: 'accounts_auth_url';
  service: 'lastfm' | 'spotify';
  url: string;
}

export type AccountService = 'lastfm' | 'listenbrainz' | 'spotify';

function empty(): AccountsSnapshot {
  return {
    kind: 'accounts_snapshot',
    lastfm: { connected: false, user: null, scrobbling: false, love_sync: false },
    listenbrainz: { submit: false, has_token: false, custom_url: null },
    spotify: { connected: false, user: null, client_id: null, redirect_port: null },
    scrobble_local: false,
  };
}

export class AccountsStore {
  model = $state<AccountsSnapshot>(empty());
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('accounts', (payload) =>
      this.#onPush(payload as AccountsSnapshot | AccountsAuthUrl),
    );
  }

  // ── connect flows ────────────────────────────────────────────────────────────────────

  connectLastfm(): void {
    this.#client.cmd('lastfm_connect', {});
  }
  connectSpotify(): void {
    this.#client.cmd('spotify_connect', {});
  }
  configureListenBrainz(fields: { submit?: boolean; token?: string; custom_url?: string }): void {
    this.#client.cmd('listen_brainz_configure', fields);
  }

  // ── field setters (uniform account_set — the demo core mutates the snapshot block) ─────

  /** value is a boolean | string | number depending on the field (write-only secrets included). */
  #set(service: AccountService, field: string, value: boolean | string | number): void {
    this.#client.cmd('account_set', { service, field, value });
  }

  setLastfmScrobbling(on: boolean): void {
    this.#set('lastfm', 'scrobbling', on);
  }
  setLastfmLoveSync(on: boolean): void {
    this.#set('lastfm', 'love_sync', on);
  }
  setSpotifyClientId(id: string): void {
    this.#set('spotify', 'client_id', id);
  }
  setSpotifyRedirectPort(port: number): void {
    this.#set('spotify', 'redirect_port', port);
  }
  setScrobbleLocal(on: boolean): void {
    this.#set('lastfm', 'scrobble_local', on);
  }

  // ── topic ──────────────────────────────────────────────────────────────────────────

  #onPush(payload: AccountsSnapshot | AccountsAuthUrl): void {
    if (payload.kind === 'accounts_auth_url') {
      // Browser-approval: hand the URL to the shell to open in the default browser.
      this.#client.win('openUrl', { url: payload.url });
      return;
    }
    if (payload.kind === 'accounts_snapshot') this.model = payload;
  }
}
