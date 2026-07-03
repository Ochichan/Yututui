// The boot payload the Rust shell injects via with_initialization_script, before the page
// loads (docs/gui/04 §3.3): window.__YTM_BOOT__. Everything after boot arrives over IPC.

import type { InstanceMode } from '../../generated/protocol/InstanceMode';

export interface BootTheme {
  /** role kebab-id → resolved hex, or the literal "none" for transparent roles. */
  roles: Record<string, string>;
  /** Optional shell-computed light/dark hint; otherwise derived from background luminance. */
  colorScheme?: 'light' | 'dark';
}

export interface DevFlags {
  /** Loaded from a Vite dev server (--dev-frontend) rather than embedded assets. */
  devFrontend: boolean;
}

export interface BootPayload {
  platform: string; // 'macos' | 'windows' | 'linux'
  version: string; // ytt-desktop build version
  coreVersion: string | null;
  protocolVersion: number;
  ownerMode: InstanceMode | null;
  locale: string; // 'en' | 'ko'
  theme: BootTheme | null;
  uiState: unknown | null; // cached UiSnapshot on rehydrate (M1+)
  devFlags: DevFlags;
}

const FALLBACK: BootPayload = {
  platform: 'unknown',
  version: '0.0.0',
  coreVersion: null,
  protocolVersion: 8,
  ownerMode: null,
  locale: 'en',
  theme: null,
  uiState: null,
  devFlags: { devFrontend: false },
};

/** Read the injected boot payload, falling back to safe defaults in a plain browser. */
export function readBoot(): BootPayload {
  const raw = (globalThis as { __YTM_BOOT__?: Partial<BootPayload> }).__YTM_BOOT__;
  if (!raw || typeof raw !== 'object') return FALLBACK;
  return { ...FALLBACK, ...raw };
}
