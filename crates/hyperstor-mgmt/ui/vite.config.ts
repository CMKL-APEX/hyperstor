import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `bun run dev` serves the UI with hot reload and passes /api and /auth
// to a hyperstor-mgmt (HYPERSTOR_MGMT_URL, default the installed one on
// loopback), which then sees the dev server's Host: start it with
// `--hosts localhost` if it differs.
const mgmt = process.env.HYPERSTOR_MGMT_URL ?? "http://127.0.0.1:8852";

/**
 * Third-party code in chunks of its own: they change only with the
 * dependencies (or the protos), so browsers keep them across releases.
 */
function manualChunks(id: string): string | undefined {
  if (/\/node_modules\/(react|react-dom|react-router|react-router-dom|scheduler)\//.test(id)) return "react";
  if (/\/node_modules\/@(bufbuild|connectrpc)\//.test(id) || id.includes("/src/gen/")) return "rpc";
  return undefined;
}

export default defineConfig({
  plugins: [react()],
  build: {
    rollupOptions: { output: { manualChunks } },
  },
  server: {
    port: 5174,
    proxy: {
      "/api": { target: mgmt, changeOrigin: false },
      "/auth": { target: mgmt, changeOrigin: false },
    },
  },
});
