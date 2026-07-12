// Transient status feedback, bottom-left above the transport bar (docs/gui/07 §19).
// Errors persist until dismissed; everything else auto-expires.

import type { Client, MutationFailure } from '../ipc/client';
import type { PushEvent } from '../../generated/protocol/PushEvent';

export type ToastSeverity = 'info' | 'success' | 'error';

export interface Toast {
  id: number;
  severity: ToastSeverity;
  text: string;
}

const TOAST_MS = 4000;
const MAX_TOASTS = 5;

export class ToastStore {
  toasts = $state<Toast[]>([]);
  #nextId = 1;
  readonly #timers = new Map<number, ReturnType<typeof setTimeout>>();

  /** Optional: system-topic pushes surface as toasts (shutdown notice today). */
  attach(client: Client): void {
    client.on('system', (payload) => {
      const ev = payload as PushEvent;
      if (ev.kind === 'shutting_down') this.show('error', 'Player is shutting down.');
    });
    client.onMutationFailure((failure) => this.show('error', mutationFailureText(failure)));
  }

  show(severity: ToastSeverity, text: string): void {
    // Repeated clicks while offline should stay visible without growing an unbounded error wall.
    if (this.toasts.some((toast) => toast.severity === severity && toast.text === text)) return;
    const id = this.#nextId++;
    this.toasts.push({ id, severity, text });
    if (this.toasts.length > MAX_TOASTS) {
      for (const removed of this.toasts.splice(0, this.toasts.length - MAX_TOASTS)) {
        this.#clearTimer(removed.id);
      }
    }
    if (severity !== 'error') {
      this.#timers.set(
        id,
        setTimeout(() => this.dismiss(id), TOAST_MS),
      );
    }
  }

  dismiss(id: number): void {
    this.#clearTimer(id);
    const i = this.toasts.findIndex((t) => t.id === id);
    if (i >= 0) this.toasts.splice(i, 1);
  }

  #clearTimer(id: number): void {
    const timer = this.#timers.get(id);
    if (timer !== undefined) clearTimeout(timer);
    this.#timers.delete(id);
  }
}

function mutationFailureText(failure: MutationFailure): string {
  switch (failure.reason) {
    case 'confirmation_lost':
      return 'The action may have been applied. Check the current state before retrying.';
    case 'busy':
    case 'must_deliver_saturated':
      return 'Player is busy. The action was not applied; try again.';
    case 'offline':
    case 'closed':
    case 'no_core':
    case 'connect_failed':
    case 'disconnected':
    case 'shutting_down':
    case 'shutdown':
      return 'Player is offline. The action was not applied.';
    case 'timeout':
      return 'Player did not confirm the action. Check its state before trying again.';
    default:
      return `Action was not applied (${failure.reason}).`;
  }
}
