import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

// No SvelteKit: this is a plain Svelte 5 SPA embedded into the WebView (Decision D2).
// vitePreprocess gives us TypeScript in <script lang="ts"> blocks.
export default {
  preprocess: vitePreprocess(),
};
