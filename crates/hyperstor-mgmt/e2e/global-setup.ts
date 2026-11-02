// Starts the cluster the tests use and tells them how to reach it
// (process.env, which the workers inherit). Stops it when the run ends:
// closing its standard input is its signal.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const workspace = join(here, "..", "..", "..");

export default async function globalSetup() {
  const target = process.env.CARGO_TARGET_DIR ?? join(workspace, "target");
  const bin = join(target, "release", "ui-e2e-cluster");
  if (!existsSync(bin)) {
    throw new Error(`${bin} is missing: cargo build --release --workspace`);
  }
  const ui = join(here, "..", "ui", "dist");
  // Primaries report their PGs' usage every half second (D155), and
  // devices their load (D218).
  const child = spawn(bin, ["--ui", ui], {
    stdio: ["pipe", "pipe", "inherit"],
    env: { ...process.env, HS_PG_USAGE_MS: "500", HS_PERF_MS: "500" },
  });
  const line = await new Promise<string>((resolve, reject) => {
    const rl = createInterface({ input: child.stdout! });
    rl.once("line", resolve);
    child.once("exit", (code) => reject(new Error(`ui-e2e-cluster exited (${code}) before starting`)));
  });
  const c = JSON.parse(line);
  process.env.HS_URL = c.url;
  process.env.HS_CLAIM = c.claim;
  process.env.HS_PASSWORD = c.password;
  process.env.HS_SSO = c.sso;
  return async () => {
    child.stdin!.end();
    await new Promise((r) => child.once("exit", r));
  };
}
