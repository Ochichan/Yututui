<script lang="ts">
  // DJ Gem chat (docs/gui/07 §12): transcript + composer. Wired to ai.svelte.ts — asks go
  // out ticketed, the `ai` topic pushes the transcript + thinking flag + playable
  // suggestions back. Suggestions play through play_tracks with one click.
  import type { AppCtx } from '../lib/ctx';

  interface Props {
    ctx: AppCtx;
  }
  const { ctx }: Props = $props();
  // svelte-ignore state_referenced_locally -- ctx is an immutable bundle; the stores inside are the reactive things
  const { ai } = ctx;

  let prompt = $state('');

  function send() {
    if (prompt.trim().length === 0) return;
    ai.ask(prompt);
    prompt = '';
  }

  const STARTERS = [
    'something upbeat for cleaning',
    '비 오는 날 코딩할 때 듣기 좋은 곡',
    'more like the last track',
  ];
</script>

<div class="ai">
  <div class="transcript" aria-label="DJ Gem conversation">
    {#if !ai.started}
      <div class="bubble assistant">
        <span class="who">✦ DJ Gem</span>
        <p>
          Hey — I'm DJ Gem. Tell me a mood, an activity, or a track and I'll pull something up. Tap
          a suggestion to play it straight away.
        </p>
      </div>
      <div class="suggestions">
        {#each STARTERS as s (s)}
          <button class="sugg" onclick={() => (prompt = s)}>{s}</button>
        {/each}
      </div>
    {:else}
      {#each ai.messages as m, i (i)}
        <div class="bubble {m.role}">
          {#if m.role === 'assistant'}<span class="who">✦ DJ Gem</span>{/if}
          <p>{m.text}</p>
        </div>
      {/each}
      {#if ai.thinking}
        <div class="bubble assistant thinking" aria-label="DJ Gem is thinking">
          <span class="who">✦ DJ Gem</span>
          <span class="dots"><i></i><i></i><i></i></span>
        </div>
      {/if}
      {#if ai.suggestions.length > 0}
        <div class="suggestions" aria-label="Suggested tracks">
          {#each ai.suggestions as t (t.video_id)}
            <button class="sugg play" title="Play" onclick={() => ai.play(t)}
              >▶ {t.display_title ?? t.title}</button
            >
          {/each}
        </div>
      {/if}
    {/if}
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
  .user {
    align-self: flex-end;
    background: var(--role-accent);
    color: var(--role-text-inverse);
    white-space: pre-wrap;
  }
  .who {
    display: block;
    margin-bottom: var(--space-1);
    font-size: 11px;
    font-weight: 700;
    color: var(--role-ai-assistant);
  }
  .thinking .dots {
    display: inline-flex;
    gap: 4px;
  }
  .thinking .dots i {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--role-text-subtle);
    animation: blink 1.2s infinite ease-in-out both;
  }
  .thinking .dots i:nth-child(2) {
    animation-delay: 0.2s;
  }
  .thinking .dots i:nth-child(3) {
    animation-delay: 0.4s;
  }
  @keyframes blink {
    0%,
    80%,
    100% {
      opacity: 0.25;
    }
    40% {
      opacity: 1;
    }
  }
  .sugg.play {
    border-color: var(--role-accent);
    color: var(--role-accent);
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
