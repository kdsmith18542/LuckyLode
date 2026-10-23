import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import pkg from "./package.json";

export default defineConfig({
  plugins: [svelte()],
  // One version source (package.json) for the About card and any future
  // build stamp — the UI used to hardcode "v0.1".
  define: { __APP_VERSION__: JSON.stringify(pkg.version) },
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: "esnext" },
});
