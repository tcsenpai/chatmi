import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// ponytail: Tauri-recommended fixed port + no clearScreen so Rust logs survive.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/sidecar/**"] },
  },
});
