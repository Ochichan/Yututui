import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

type PanelMessage = {
  v: number;
  id: number;
  command: { action: string; value?: unknown };
};

type CommandResult = {
  id: number;
  ok: boolean;
  error?: { code: string; displayMessage: string; retryable: boolean };
};

type QueueItem = {
  index: number;
  title: string;
  artist: string;
  duration: string;
  current: boolean;
};

type PanelPayload = ReturnType<typeof playingPayload>;

type PanelWindow = Window & {
  ipc: { postMessage(message: string): void };
  ytmTuiApply(payload: PanelPayload): void;
  ytmTuiApplyArt(uri: string | null): void;
  ytmTuiCommandResult(result: CommandResult): void;
  ytmTuiFocusPrimary(): void;
};

const panelRoot = resolve(process.cwd(), '../src/desktop');
const panelSource = [
  'panel_assets/document_start.html',
  'panel_assets/common.css',
  'panel_assets/cushion.css',
  'panel_assets/shared.css',
  'panel_assets/minimal.css',
  'panel_assets/tamagotchi.css',
  'panel_assets/accessibility.css',
  'panel_assets/body_start.html',
  'panel.html',
  'panel_assets/script_start.html',
  'panel_assets/ipc-state.js',
  'panel_assets/document_end.html',
]
  .map((path) => readFileSync(resolve(panelRoot, path), 'utf8'))
  .join('');

function playingPayload() {
  return {
    locale: 'en',
    connected: true,
    state: 'playing',
    title: 'Test Song',
    artist: 'Test Artist',
    stateLabel: 'Playing',
    ownerLabel: 'Daemon',
    queueLabel: '1 / 2',
    volumeLabel: '63%',
    volume: 63,
    elapsedMs: 42_000,
    durationMs: 180_000,
    isLive: false,
    queueRev: 41 as number | null,
    trackIdentity: 'v8\u001ftest-track\u001f7',
    canSeek: true,
    queue: [
      {
        index: 0,
        title: 'Test Song',
        artist: 'Test Artist',
        duration: '3:00',
        current: true,
      },
      {
        index: 1,
        title: 'Second Song',
        artist: 'Second Artist',
        duration: '2:30',
        current: false,
      },
    ] satisfies QueueItem[],
    shuffle: false,
    repeat: 'off',
    repeatLabel: 'Off',
    paused: false,
    streaming: false,
    error: null as string | null,
    canPlayback: true,
    canVolume: true,
    canManageQueue: true,
    canToggleStreaming: true,
    canStartDaemon: false,
    canResumeDaemon: false,
    canStopDaemon: true,
    settings: {
      autoplayStreaming: false,
      streamingMode: 'balanced',
      streamingModeLabel: 'Balanced',
      streamingSource: 'youtube',
      streamingSourceLabel: 'YouTube',
      streamingSources: [{ value: 'youtube', label: 'YouTube' }],
      speedTenths: 10,
      speedLabel: '1.0x',
      seekSeconds: 10,
      seekLabel: '10s',
      normalize: false,
      gapless: true,
      aiEnabled: true,
      radioMode: false,
      canRadioMode: true,
    },
  };
}

function panelWindow(): PanelWindow {
  return window as unknown as PanelWindow;
}

function commandMessages(postMessage: ReturnType<typeof vi.fn>): PanelMessage[] {
  return postMessage.mock.calls
    .map(([message]) => JSON.parse(String(message)) as Partial<PanelMessage>)
    .filter((message): message is PanelMessage => message.v === 1 && message.command != null);
}

function messagesFor(postMessage: ReturnType<typeof vi.fn>, action: string): PanelMessage[] {
  return commandMessages(postMessage).filter((message) => message.command.action === action);
}

function keydown(element: EventTarget, key: string): void {
  element.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key }));
}

function keyup(element: EventTarget, key: string): void {
  element.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true, cancelable: true, key }));
}

function clickWithDetail(element: EventTarget, detail: number): void {
  element.dispatchEvent(
    new MouseEvent('click', {
      bubbles: true,
      cancelable: true,
      detail,
    }),
  );
}

function pointerEvent(type: string, clientX: number): MouseEvent {
  const event = new MouseEvent(type, { bubbles: true, cancelable: true, clientX });
  Object.defineProperty(event, 'pointerId', { configurable: true, value: 1 });
  return event;
}

function element(id: string): HTMLElement {
  const found = document.getElementById(id);
  if (!found) throw new Error(`panel element #${id} is missing`);
  return found;
}

function bootPanel(
  options: {
    payload?: PanelPayload;
    theme?: 'default' | 'minimal' | 'tamagotchi';
    expanded?: boolean;
    sharedSheet?: 'queue' | 'more' | null;
    queueScrollY?: number;
    activeControl?: string | null;
  } = {},
): ReturnType<typeof vi.fn> {
  const payload = options.payload ?? playingPayload();
  const theme = options.theme ?? 'default';
  const rendered = panelSource
    .replace('__INITIAL_PAYLOAD__', JSON.stringify(payload))
    .replace('__PANEL_THEME__', theme)
    .replace('__PANEL_LANG__', payload.locale)
    .replace('__PANEL_LOCALE__', payload.locale)
    .replace('__INITIAL_ART__', 'null')
    .replace('__INITIAL_PINNED__', 'false')
    .replace('__INITIAL_EXPANDED__', String(options.expanded ?? false))
    .replace('__INITIAL_SHARED_SHEET__', JSON.stringify(options.sharedSheet ?? null))
    .replace('__INITIAL_QUEUE_SCROLL_Y__', String(options.queueScrollY ?? 0))
    .replace('__INITIAL_ACTIVE_CONTROL__', JSON.stringify(options.activeControl ?? null))
    .replaceAll('__CSP_NONCE__', 'test-nonce');
  const scriptMatch = rendered.match(/<script nonce="test-nonce">([\s\S]*?)<\/script>/);
  if (!scriptMatch) throw new Error('panel script was not found');

  document.open();
  document.write(rendered.replace(scriptMatch[0], ''));
  document.close();

  const postMessage = vi.fn();
  panelWindow().ipc = { postMessage };
  vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
  window.requestAnimationFrame = (callback: FrameRequestCallback): number => {
    callback(performance.now());
    return 1;
  };
  Object.defineProperty(HTMLElement.prototype, 'offsetParent', {
    configurable: true,
    get() {
      return (this as HTMLElement).hidden ? null : document.body;
    },
  });
  Object.defineProperty(Element.prototype, 'setPointerCapture', {
    configurable: true,
    value: () => undefined,
  });

  // Evaluate the exact embedded script. The wrapper isolates its lexical bindings
  // so each test can boot a fresh document in the shared happy-dom Window.
  window.eval(`(() => {${scriptMatch[1]}\n})()`);
  return postMessage;
}

describe('desktop mini-player embedded panel', () => {
  beforeEach(() => {
    vi.useFakeTimers({ now: 10_000 });
  });

  afterEach(() => {
    vi.clearAllTimers();
    vi.restoreAllMocks();
    vi.useRealTimers();
    document.open();
    document.write('<!doctype html><html><body></body></html>');
    document.close();
  });

  it('boots with a correlated v1 FrontendReady request', () => {
    const postMessage = bootPanel();

    expect(commandMessages(postMessage)).toEqual([
      { v: 1, id: 1, command: { action: 'frontend_ready' } },
    ]);
  });

  it('coalesces repeated volume keys and flushes the final value on key-up', () => {
    const postMessage = bootPanel();
    const volume = element('volumeBar');

    expect(volume.getAttribute('role')).toBe('slider');
    expect(volume.getAttribute('aria-disabled')).toBe('false');
    expect(volume.tabIndex).toBe(0);
    for (let i = 0; i < 5; i += 1) keydown(volume, 'ArrowRight');
    expect(messagesFor(postMessage, 'set_volume')).toHaveLength(0);
    expect(volume.getAttribute('aria-valuenow')).toBe('68');
    keyup(volume, 'ArrowRight');
    expect(messagesFor(postMessage, 'set_volume').map((message) => message.command.value)).toEqual([
      68,
    ]);

    panelWindow().ytmTuiApply({ ...playingPayload(), canVolume: false });
    const countBeforeDisabledKey = messagesFor(postMessage, 'set_volume').length;
    expect(volume.getAttribute('aria-disabled')).toBe('true');
    expect(volume.tabIndex).toBe(-1);
    keydown(volume, 'ArrowRight');
    expect(messagesFor(postMessage, 'set_volume')).toHaveLength(countBeforeDisabledKey);
  });

  it('flushes the latest 63 to 68 volume drag value on pointer-up', () => {
    const postMessage = bootPanel();
    const volume = element('volumeBar');
    Object.defineProperty(volume, 'getBoundingClientRect', {
      configurable: true,
      value: () =>
        ({
          x: 0,
          y: 0,
          top: 0,
          right: 100,
          bottom: 20,
          left: 0,
          width: 100,
          height: 20,
          toJSON: () => ({}),
        }) satisfies DOMRect,
    });

    volume.dispatchEvent(pointerEvent('pointerdown', 63));
    volume.dispatchEvent(pointerEvent('pointermove', 68));
    expect(messagesFor(postMessage, 'set_volume')).toHaveLength(0);
    volume.dispatchEvent(pointerEvent('pointerup', 68));

    expect(messagesFor(postMessage, 'set_volume').map((message) => message.command.value)).toEqual([
      68,
    ]);
  });

  it('closes a compact shared sheet before Escape hides the panel', () => {
    const postMessage = bootPanel({ theme: 'minimal' });
    element('tabQueue').click();

    expect(document.documentElement.classList.contains('shared-sheet')).toBe(true);
    expect(element('sharedSheetTitle').textContent).toBe('Queue');
    expect(messagesFor(postMessage, 'set_shared_sheet').at(-1)?.command.value).toBe('queue');

    keydown(document, 'Escape');
    expect(document.documentElement.classList.contains('shared-sheet')).toBe(false);
    expect(messagesFor(postMessage, 'set_shared_sheet').at(-1)?.command.value).toBe(false);
    expect(messagesFor(postMessage, 'hide')).toHaveLength(0);

    keydown(document, 'Escape');
    expect(messagesFor(postMessage, 'hide')).toHaveLength(1);
  });

  it('keeps pending state until the matching correlated error result arrives', () => {
    const postMessage = bootPanel();
    const shuffle = element('shuffle');
    shuffle.click();
    const request = messagesFor(postMessage, 'toggle_shuffle').at(-1);
    if (!request) throw new Error('shuffle request was not sent');

    expect(shuffle.classList.contains('pending')).toBe(true);
    expect(shuffle.getAttribute('aria-busy')).toBe('true');
    panelWindow().ytmTuiCommandResult({ id: request.id + 10, ok: true });
    expect(shuffle.classList.contains('pending')).toBe(true);

    panelWindow().ytmTuiCommandResult({
      id: request.id,
      ok: false,
      error: {
        code: 'incompatible_playback_modes',
        displayMessage: 'Autoplay and repeat cannot be combined.',
        retryable: false,
      },
    });
    expect(shuffle.classList.contains('pending')).toBe(false);
    expect(shuffle.hasAttribute('aria-busy')).toBe(false);
    expect(element('error').hidden).toBe(false);
    expect(element('error').textContent).toBe('Autoplay and repeat cannot be combined.');
    expect(element('liveRegion').textContent).toBe('Autoplay and repeat cannot be combined.');
    expect(element('error').tabIndex).toBe(0);
    expect(element('error').title).toBe('Autoplay and repeat cannot be combined.');
  });

  it('keeps a queue row locked when an authoritative push rebuilds its DOM', () => {
    const postMessage = bootPanel();
    const original = document.querySelector<HTMLButtonElement>('.queue-track[data-position="0"]');
    if (!original) throw new Error('queue play button was not rendered');
    original.click();
    const request = messagesFor(postMessage, 'queue_play').at(-1);
    if (!request) throw new Error('queue play request was not sent');

    const changed = playingPayload();
    changed.queueRev = 42;
    changed.queue[0] = { ...changed.queue[0]!, title: 'Renamed Song' };
    panelWindow().ytmTuiApply(changed);

    const replacement = document.querySelector<HTMLButtonElement>(
      '.queue-track[data-position="0"]',
    );
    if (!replacement) throw new Error('replacement queue play button was not rendered');
    expect(replacement).not.toBe(original);
    expect(replacement.classList.contains('pending')).toBe(true);
    expect(replacement.getAttribute('aria-busy')).toBe('true');
    expect(replacement.getAttribute('aria-disabled')).toBe('true');

    replacement.click();
    expect(messagesFor(postMessage, 'queue_play')).toHaveLength(1);

    panelWindow().ytmTuiCommandResult({ id: request.id, ok: true });
    expect(replacement.classList.contains('pending')).toBe(false);
    expect(replacement.hasAttribute('aria-busy')).toBe(false);
    expect(replacement.hasAttribute('aria-disabled')).toBe(false);
  });

  it('updates document language, labels, and queue accessible names at runtime', () => {
    bootPanel();
    const korean = { ...playingPayload(), locale: 'ko', queueRev: 42 };
    panelWindow().ytmTuiApply(korean);

    expect(document.documentElement.lang).toBe('ko');
    expect(element('tabQueue').textContent).toBe('대기열');
    expect(element('volumeBar').getAttribute('aria-label')).toBe('음량');
    expect(element('compactMenu').getAttribute('aria-label')).toBe('추가 제어');
    expect(document.querySelector('.queue-remove')?.getAttribute('aria-label')).toBe(
      '삭제 Test Song',
    );

    panelWindow().ytmTuiApply({ ...korean, locale: 'en' });
    expect(document.documentElement.lang).toBe('en');
    expect(element('tabQueue').textContent).toBe('Queue');
    expect(document.querySelector('.queue-remove')?.getAttribute('aria-label')).toBe(
      'Remove Test Song',
    );
  });

  it('requires a later pointer sequence before removing a queue row', () => {
    const postMessage = bootPanel();
    const remove = document.querySelector<HTMLButtonElement>('.queue-remove');
    if (!remove) throw new Error('queue remove button was not rendered');

    remove.focus();
    clickWithDetail(remove, 1);
    expect(remove.disabled).toBe(false);
    expect(remove.getAttribute('aria-disabled')).toBe('true');
    expect(document.activeElement).toBe(remove);
    expect(remove.getAttribute('aria-label')).toBe('Confirm removal of Test Song');
    expect(messagesFor(postMessage, 'queue_remove')).toHaveLength(0);

    // The guard receives but rejects the rest of this pointer sequence.
    clickWithDetail(remove, 2);
    expect(messagesFor(postMessage, 'queue_remove')).toHaveLength(0);

    vi.advanceTimersByTime(450);
    expect(remove.disabled).toBe(false);
    expect(remove.hasAttribute('aria-disabled')).toBe(false);
    expect(document.activeElement).toBe(remove);
    // Even when the OS double-click window exceeds 450 ms, detail=2 identifies
    // this as the same pointer sequence and must not confirm the destructive action.
    clickWithDetail(remove, 2);
    expect(messagesFor(postMessage, 'queue_remove')).toHaveLength(0);

    clickWithDetail(remove, 1);
    expect(messagesFor(postMessage, 'queue_remove').at(-1)?.command.value).toEqual({
      position: 0,
      expectedRev: 41,
    });
  });

  it('serializes player lifecycle actions behind one shared pending lock', () => {
    const payload = {
      ...playingPayload(),
      connected: false,
      state: 'disconnected',
      stateLabel: 'Disconnected',
      canPlayback: false,
      canVolume: false,
      canSeek: false,
      canStartDaemon: true,
      canResumeDaemon: true,
      canStopDaemon: false,
    };
    const postMessage = bootPanel({ payload });
    const resume = element('resumeDaemon');
    const start = element('startDaemon');

    resume.click();
    const request = messagesFor(postMessage, 'resume_daemon').at(-1);
    if (!request) throw new Error('resume request was not sent');
    expect(start.classList.contains('pending')).toBe(true);
    expect(element('stopDaemon').classList.contains('pending')).toBe(true);

    start.click();
    expect(messagesFor(postMessage, 'start_daemon')).toHaveLength(0);

    panelWindow().ytmTuiCommandResult({ id: request.id, ok: true });
    expect(document.querySelectorAll('[data-action$="_daemon"].pending')).toHaveLength(0);
    start.click();
    expect(messagesFor(postMessage, 'start_daemon')).toHaveLength(1);
  });

  it('keeps a legacy queue without a revision read-only', () => {
    const payload = { ...playingPayload(), queueRev: null, canManageQueue: false };
    const postMessage = bootPanel({ payload });
    const play = document.querySelector<HTMLButtonElement>('.queue-track');
    const remove = document.querySelector<HTMLButtonElement>('.queue-remove');
    if (!play || !remove) throw new Error('queue controls were not rendered');

    expect(play.disabled).toBe(true);
    expect(remove.disabled).toBe(true);
    play.click();
    remove.click();
    expect(messagesFor(postMessage, 'queue_play')).toHaveLength(0);
    expect(messagesFor(postMessage, 'queue_remove')).toHaveLength(0);
  });

  it('serializes rapid keyboard theme changes until the correlated result', () => {
    const postMessage = bootPanel();
    const current = document.querySelector<HTMLElement>(
      '.theme-pick .theme-opt[data-value="default"]',
    );
    if (!current) throw new Error('default theme option is missing');
    current.focus();

    keydown(current, 'ArrowRight');
    keydown(current, 'ArrowRight');
    const requests = messagesFor(postMessage, 'set_theme');
    expect(requests).toHaveLength(1);
    expect(requests[0]?.command.value).toBe('minimal');
    expect(document.documentElement.dataset.theme).toBe('default');

    panelWindow().ytmTuiCommandResult({ id: requests[0]!.id, ok: true });
    expect(document.documentElement.dataset.theme).toBe('minimal');
    expect(document.querySelectorAll('.theme-opt.pending')).toHaveLength(0);
  });

  it('keeps the previous theme and unlocks every picker after rejection', () => {
    const postMessage = bootPanel();
    const option = document.querySelector<HTMLElement>(
      '.theme-pick .theme-opt[data-value="tamagotchi"]',
    );
    if (!option) throw new Error('Tama theme option is missing');
    option.click();
    const request = messagesFor(postMessage, 'set_theme').at(-1);
    if (!request) throw new Error('theme request was not sent');

    panelWindow().ytmTuiCommandResult({
      id: request.id,
      ok: false,
      error: { code: 'persist_failed', displayMessage: 'Could not save theme', retryable: true },
    });
    expect(document.documentElement.dataset.theme).toBe('default');
    expect(document.querySelectorAll('.theme-opt.pending')).toHaveLength(0);
    expect(document.querySelectorAll('.theme-opt[aria-busy="true"]')).toHaveLength(0);
  });

  it('restores focus to the active skin transport when its theme picker is no longer visible', () => {
    const postMessage = bootPanel();
    const options = Array.from(
      document.querySelectorAll<HTMLElement>('.theme-opt[data-value="tamagotchi"]'),
    );
    const source = options[0];
    if (!source) throw new Error('Tama theme option is missing');
    for (const option of options) {
      Object.defineProperty(option, 'offsetParent', {
        configurable: true,
        get: () => null,
      });
    }

    source.focus();
    source.click();
    expect(document.documentElement.dataset.theme).toBe('default');
    const request = messagesFor(postMessage, 'set_theme').at(-1);
    if (!request) throw new Error('set_theme request was not sent');
    panelWindow().ytmTuiCommandResult({ id: request.id, ok: true });

    expect(document.documentElement.dataset.theme).toBe('tamagotchi');
    expect(element('sharedTransport').parentElement).toBe(element('transportSlot'));
    expect(document.activeElement).toBe(element('playPause'));
  });

  it('opens the common Tama controls and preserves directional tab navigation', () => {
    bootPanel({ theme: 'tamagotchi' });
    element('compactMenu').click();

    expect(element('playerRoot').classList.contains('menu')).toBe(true);
    expect(element('compactMenu').getAttribute('aria-expanded')).toBe('true');
    expect(document.activeElement).toBe(element('shuffle'));
    expect(element('tabQueue').tabIndex).toBe(0);
    element('tabQueue').focus();
    keydown(element('tabQueue'), 'ArrowRight');
    expect(document.documentElement.classList.contains('shared-sheet')).toBe(true);
    expect(element('sharedSheetTitle').textContent).toBe('More');
    expect(document.activeElement).toBe(element('sharedSheetBack'));
  });

  it('keeps one shared skin state available to assistive technology', () => {
    bootPanel({ theme: 'minimal' });

    expect(element('stateLabel').textContent).toBe('Playing');
    expect(element('playerRoot').getAttribute('aria-label')).toBe(
      'Playing. Test Song by Test Artist',
    );
    expect(element('tamaVisual').getAttribute('aria-hidden')).toBe('true');
    const root = element('playerRoot');
    for (const id of [
      'stateLabel',
      'title',
      'artist',
      'artImg',
      'recovery',
      'progressBar',
      'volumeBar',
      'shuffle',
      'repeat',
      'pin',
      'hide',
      'tabQueue',
      'tabMore',
      'panelQueue',
      'panelMore',
    ]) {
      expect(root.contains(element(id)), `#${id} must belong to the common tree`).toBe(true);
    }
    expect(
      element('tamaVisual').querySelectorAll('button, input, select, [tabindex]'),
    ).toHaveLength(0);
  });

  it('focuses the active skin primary transport or recovery CTA', () => {
    bootPanel({ theme: 'minimal' });
    panelWindow().ytmTuiFocusPrimary();
    expect(element('sharedTransport').parentElement).toBe(element('transportSlot'));
    expect(document.activeElement).toBe(element('playPause'));

    const offline = {
      ...playingPayload(),
      connected: false,
      state: 'disconnected',
      stateLabel: 'Disconnected',
      canPlayback: false,
      canVolume: false,
      canSeek: false,
      canStartDaemon: true,
      canResumeDaemon: true,
      canStopDaemon: false,
      queue: [] as QueueItem[],
      queueRev: 42,
    };
    panelWindow().ytmTuiApply(offline);
    panelWindow().ytmTuiFocusPrimary();
    expect(document.activeElement?.getAttribute('data-recovery')).toBe('resume');
  });

  it('preserves full CJK, emoji, and RTL track text for tooltips and accessible names', () => {
    bootPanel();
    const payload = playingPayload();
    payload.title = '아주 긴 재생 제목 🎧 مرحبا بالعالم';
    payload.artist = '아티스트 이름 — שלום';
    payload.queueRev = 42;
    payload.queue[0] = {
      ...payload.queue[0]!,
      title: '대기열의 매우 긴 곡 제목 🌙 العربية',
      artist: '가수 이름 שלום',
    };
    panelWindow().ytmTuiApply(payload);

    expect(element('title').textContent).toBe(payload.title);
    expect(element('title').getAttribute('dir')).toBe('auto');
    expect(element('title').title).toBe(payload.title);
    expect(element('artist').getAttribute('dir')).toBe('auto');
    expect(element('artist').title).toBe(payload.artist);
    const queueTitle = document.querySelector<HTMLElement>('.queue-title');
    const queuePlay = document.querySelector<HTMLElement>('.queue-track');
    expect(queueTitle?.getAttribute('dir')).toBe('auto');
    expect(queueTitle?.title).toBe(payload.queue[0]!.title);
    expect(queuePlay?.getAttribute('aria-label')).toBe(`Play ${payload.queue[0]!.title}`);
  });

  it('reveals and flashes Tama artwork only after pixelation and decode complete', async () => {
    const originalImage = Object.getOwnPropertyDescriptor(window, 'Image');
    const originalDecode = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'decode');
    const preloader = document.createElement('img');
    const decodeGate: { resolve?: () => void } = {};
    Object.defineProperty(window, 'Image', {
      configurable: true,
      value: function Image() {
        return preloader;
      },
    });
    Object.defineProperty(HTMLImageElement.prototype, 'decode', {
      configurable: true,
      value: vi.fn(
        () =>
          new Promise<void>((resolve) => {
            decodeGate.resolve = resolve;
          }),
      ),
    });
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({
      drawImage: vi.fn(),
    } as unknown as CanvasRenderingContext2D);
    vi.spyOn(HTMLCanvasElement.prototype, 'toDataURL').mockReturnValue(
      'data:image/png;base64,cGl4ZWxhdGVk',
    );

    try {
      bootPanel({ theme: 'tamagotchi' });
      panelWindow().ytmTuiApplyArt('data:image/png;base64,bmV3LWNvdmVy');
      expect((element('artImg') as HTMLImageElement).hidden).toBe(true);
      expect(element('playerRoot').classList.contains('art-flash')).toBe(false);

      preloader.onload?.(new Event('load'));
      expect((element('artImg') as HTMLImageElement).hidden).toBe(true);
      expect(decodeGate.resolve).toBeTypeOf('function');
      decodeGate.resolve?.();
      await Promise.resolve();
      await Promise.resolve();

      expect((element('artImg') as HTMLImageElement).hidden).toBe(false);
      expect(element('playerRoot').classList.contains('art-flash')).toBe(true);
      expect(element('tmScreen').classList.contains('flash')).toBe(true);

      panelWindow().ytmTuiApplyArt('data:image/png;base64,bmV4dC1jb3Zlcg==');
      expect((element('artImg') as HTMLImageElement).hidden).toBe(true);
      expect(element('playerRoot').classList.contains('art-flash')).toBe(false);
      expect(element('tmScreen').classList.contains('flash')).toBe(false);
    } finally {
      if (originalImage) Object.defineProperty(window, 'Image', originalImage);
      if (originalDecode) {
        Object.defineProperty(HTMLImageElement.prototype, 'decode', originalDecode);
      } else {
        delete (HTMLImageElement.prototype as Partial<HTMLImageElement>).decode;
      }
    }
  });

  it('restores and re-caches bounded queue scroll and focus across a page rebuild', () => {
    const postMessage = bootPanel({
      theme: 'minimal',
      sharedSheet: 'queue',
      queueScrollY: 421,
      activeControl: 'queue-remove-1',
    });

    expect(element('queueList').scrollTop).toBe(421);
    expect(document.activeElement).toBe(element('queue-remove-1'));

    element('queueList').scrollTop = 512;
    element('queue-play-0').focus();
    element('queueList').dispatchEvent(new Event('scroll'));
    vi.advanceTimersByTime(100);

    const snapshots = postMessage.mock.calls
      .map(([message]) => JSON.parse(message as string) as Record<string, unknown>)
      .filter((message) => message.action === 'persist_ui');
    expect(snapshots.at(-1)).toEqual({
      action: 'persist_ui',
      value: { queueScrollY: 512, activeControl: 'queue-play-0' },
    });
  });
});
