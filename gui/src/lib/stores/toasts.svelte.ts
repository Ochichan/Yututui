// Transient status feedback, bottom-left above the transport bar (docs/gui/07 §19).
// Errors persist until dismissed; everything else auto-expires.

import type { Client } from '../ipc/client';
import type { PushEvent } from '../../generated/protocol/PushEvent';

export type ToastSeverity = 'info' | 'success' | 'error';

export interface Toast {
  id: number;
  severity: ToastSeverity;
  text: string;
}

const TOAST_MS = 4000;

export class ToastStore {
  toasts = $state<Toast[]>([]);
  #nextId = 1;

  /** Optional: system-topic pushes surface as toasts (shutdown notice today). */
  attach(client: Client): void {
    client.on('system', (payload) => {
      const ev = payload as PushEvent;
      if (ev.kind === 'shutting_down') this.show('error', 'Player is shutting down.');
    });
  }

  show(severity: ToastSeverity, text: string): void {
    const id = this.#nextId++;
    this.toasts.push({ id, severity, text });
    if (severity !== 'error') {
      setTimeout(() => this.dismiss(id), TOAST_MS);
    }
  }

  dismiss(id: number): void {
    const i = this.toasts.findIndex((t) => t.id === id);
    if (i >= 0) this.toasts.splice(i, 1);
  }
}
