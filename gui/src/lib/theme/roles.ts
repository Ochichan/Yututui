// The 34 theme roles (kebab ids matching ThemeRole::id(), src/theme.rs:279-313) with
// human labels. Single source for the Graphics editor rows and the local-theme
// completeness test — a palette missing any of these fails CI.

export const ROLES: Array<[id: string, label: string]> = [
  ['background', 'Background'],
  ['text-primary', 'Text · primary'],
  ['text-muted', 'Text · muted'],
  ['text-subtle', 'Text · subtle'],
  ['text-inverse', 'Text · inverse'],
  ['border-primary', 'Border · primary'],
  ['border-focused', 'Border · focused'],
  ['border-muted', 'Border · muted'],
  ['accent', 'Accent'],
  ['accent-alt', 'Accent · alt'],
  ['success', 'Success'],
  ['warning', 'Warning'],
  ['error', 'Error'],
  ['selection-fg', 'Selection · fg'],
  ['selection-bg', 'Selection · bg'],
  ['selection-inactive-fg', 'Selection inactive · fg'],
  ['selection-inactive-bg', 'Selection inactive · bg'],
  ['gauge-filled', 'Gauge · filled'],
  ['gauge-empty', 'Gauge · empty'],
  ['player-control', 'Player · control'],
  ['player-label', 'Player · label'],
  ['help-group', 'Help · group'],
  ['help-key', 'Help · key'],
  ['help-action', 'Help · action'],
  ['settings-group', 'Settings · group'],
  ['settings-label', 'Settings · label'],
  ['settings-value', 'Settings · value'],
  ['settings-value-focused', 'Settings · value focused'],
  ['ai-user', 'AI · user'],
  ['ai-assistant', 'AI · assistant'],
  ['ai-error', 'AI · error'],
  ['ai-thinking', 'AI · thinking'],
  ['lyrics-current', 'Lyrics · current'],
  ['lyrics-dim', 'Lyrics · dim'],
];

export const ROLE_IDS = ROLES.map(([id]) => id);
