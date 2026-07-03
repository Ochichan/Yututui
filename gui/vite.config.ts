import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The dist embeds into ytt-desktop and loads offline from `ytm://app/...`, so:
//  - absolute asset paths (we own the origin) — do NOT set base: './'
//  - one JS + one CSS chunk keeps the build.rs asset table trivial
//  - target the two shipped WebViews (WKWebView on macOS 13+, evergreen WebView2)
export default defineConfig({
  plugins: [svelte()],
  build: {
    target: ['safari16', 'chrome110'],
    outDir: 'dist',
    assetsDir: 'assets',
    rollupOptions: {
      output: { manualChunks: undefined },
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['tests/**/*.test.ts'],
  },
});
