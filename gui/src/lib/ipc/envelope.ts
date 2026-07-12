// The IPC wire envelope between the Svelte frontend and the Rust shell (docs/gui/05 §4.1).
//
// The envelope is a thin, hand-authored contract — NOT a ts-rs generated type. Its payloads
// are the generated protocol models; the envelope only frames them.

/** Frontend → Rust (bridge.rs). */
export type OutKind = 'cmd' | 'req' | 'sub' | 'unsub' | 'win';

/** Rust → frontend (evaluate_script → window.__ytm.receive). */
export type InKind = 'res' | 'err' | 'event' | 'conn';

export interface OutEnvelope {
  v: 1;
  /** Present for every user `cmd` and correlated `req`; absent for subscriptions/window ops. */
  id?: number;
  /** Page/WebView lifetime namespace; omitted only by legacy embedded frontends. */
  page_id?: string;
  /** Stable per-page command/request identity used by the core's deduplication registry. */
  request_id?: string;
  kind: OutKind;
  /** Command / request / window-op name, or the topic verb for sub/unsub. */
  name: string;
  payload?: unknown;
}

export interface InEnvelope {
  v: 1;
  /** Echoes the triggering `cmd`/`req` id on `res`/`err`. */
  id?: number;
  /** Echoes the triggering page on correlated replies; absent from legacy shell replies. */
  page_id?: string;
  kind: InKind;
  /** Set on `event` pushes: the topic wire string. */
  topic?: string;
  payload?: unknown;
}

/** Payload of a `conn` envelope — drives the reconnect state machine (docs/gui/05 §4.2). */
export interface ConnPayload {
  state: ConnState;
  coreVersion?: string | null;
  protocolVersion?: number;
  capabilities?: string[];
  ownerMode?: string;
  /** Machine reason for degraded/offline (clients key off this, never a display string). */
  reason?: string;
}

export type ConnState = 'connecting' | 'online' | 'degraded' | 'offline';
