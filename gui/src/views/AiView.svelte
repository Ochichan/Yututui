<script lang="ts">
  // DJ Gem chat (docs/gui/07 §12). The transcript/suggestions wire (ticketed AskAi + the
  // `ai` topic) is pending — so DJ Gem's first message *is* the patch-bay notice, in
  // character, inside the finished chat frame.
  import type { AppCtx } from '../lib/ctx';
  import { WIRING } from '../lib/wiring/registry';
  import WireTag from '../lib/components/WireTag.svelte';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { wip } = ctx;

  let prompt = $state('');

  function send() {
    if (prompt.trim().length === 0) return;
    // TODO(wire:M4/ai.chat): replace with ai.svelte.ts ask(ticket, prompt) once wired;
    // the gate auto-opens on the `ai` capability.
    wip.gate('ai.chat');
  }

  const SUGGESTION_HINTS = [
    'something upbeat for cleaning',
    '비 오는 날 코딩할 때 듣기 좋은 곡',
    'more like the last track',
  ];
</script>

<div class="ai">
  <div class="transcript" aria-label="DJ Gem conversation">
    <div class="bubble assistant">
      <span class="who">✦ DJ Gem</span>
      <p>
        Hey — I'm DJ Gem. My chat wire hasn't been patched into the core yet ({WIRING['ai.chat']
          .milestone} brings it). Once it lands I'll take requests, explain my autoplay picks, and drop
        suggestions you can play with one click.
      </p>
      <p class="meta-line">
        <WireTag id="ai.chat" {wip} />
      </p>
    </div>

    <div class="suggestions" aria-hidden="true">
      {#each SUGGESTION_HINTS as s (s)}
        <button class="sugg" onclick={() => (prompt = s)}>{s}</button>
      {/each}
    </div>
  </div>

  <form
    class="composer"
    onsubmit={(e) => {
      e.preventDefault();
      send();
    }}
  >
    <input
      class="ti"
      placeholder="Ask DJ Gem for music…"
      bind:value={prompt}
      aria-label="Message DJ Gem"
    />
    <button class="send" type="submit" title="Send">➤</button>
  </form>
</div>

<style>
  .ai {
    display: flex;
    flex-direction: column;
    height: 100%;
    padding: var(--space-6) var(--space-8);
    gap: var(--space-4);
  }
  .transcript {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .bubble {
    max-width: 560px;
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-l);
    font-size: 13px;
    line-height: 1.55;
  }
  .bubble p {
    margin: 0 0 var(--space-2);
  }
  .bubble p:last-child {
    margin-bottom: 0;
  }
  .assistant {
    align-self: flex-start;
    background: var(--surface-1);
    border: 1px solid var(--role-border-muted);
    border-left: 3px solid var(--role-ai-assistant);
  }
  .who {
    display: block;
    margin-bottom: var(--space-1);
    font-size: 11px;
    font-weight: 700;
    color: var(--role-ai-assistant);
  }
  .meta-line {
    margin: 0;
  }
  .suggestions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    padding-left: var(--space-2);
  }
  .sugg {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--role-border-muted);
    border-radius: var(--radius-pill);
    background: transparent;
    color: var(--role-text-muted);
    font-size: 12px;
  }
  .sugg:hover {
    background: var(--surface-2);
    color: var(--role-text-primary);
  }
  .composer {
    display: flex;
    gap: var(--space-2);
  }
  .composer input {
    flex: 1;
    height: 42px;
  }
  .send {
    width: 42px;
    height: 42px;
    border: none;
    border-radius: var(--radius-pill);
    background: var(--role-accent);
    color: var(--role-text-inverse);
    font-size: 15px;
  }
  .send:hover {
    background: var(--role-accent-alt);
  }
</style>
