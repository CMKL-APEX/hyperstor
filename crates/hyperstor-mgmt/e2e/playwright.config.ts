import { defineConfig } from "@playwright/test";

// The web UI against a real cluster: global-setup.ts starts one
// (ui-e2e-cluster, from the release build) serving ../ui/dist. One worker:
// the tests share the cluster and its people, in order.
export default defineConfig({
  testDir: "tests",
  globalSetup: "./global-setup.ts",
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  retries: 0,
  reporter: [["list"], ["./cargo-reporter.ts"], ...(process.env.CI ? ([["html", { open: "never" }]] as const) : [])],
  use: {
    headless: true,
    trace: "retain-on-failure",
  },
});
