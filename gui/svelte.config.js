import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

// No SvelteKit: this is a plain Svelte 5 SPA embedded into the WebView (Decision D2).
// vitePreprocess gives us TypeScript in <script lang="ts"> blocks. Style preprocessing
// is off: every <style> block is plain CSS (design tokens, docs/gui/06), and the style
// pass trips a vite-in-vitest incompatibility when component tests mount .svelte files.
export default {
  preprocess: vitePreprocess({ style: false }),
};
