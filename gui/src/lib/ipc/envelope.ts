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
  /** Present for correlated `req` (and the `win` persist reply); absent for fire-and-forget. */
  id?: number;
  kind: OutKind;
  /** Command / request / window-op name, or the topic verb for sub/unsub. */
  name: string;
  payload?: unknown;
}

export interface InEnvelope {
  v: 1;
  /** Echoes the triggering `req` id on `res`/`err`. */
  id?: number;
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
