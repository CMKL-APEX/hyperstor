// Writes the browser tests' results as `cargo test` prints them, to
// test-results/cargo.log, so scripts/tracking.py checks them with the
// rest: `browser::<title in snake case>`.
import type { FullResult, Reporter, TestCase, TestResult } from "@playwright/test/reporter";
import { mkdirSync, writeFileSync } from "node:fs";

const name = (t: TestCase) =>
  t.title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_|_$/g, "");

export default class CargoReporter implements Reporter {
  private lines: string[] = ["     Running tests/ui.spec.ts (e2e/deps/browser-0)"];

  onTestEnd(test: TestCase, result: TestResult) {
    const status = result.status === "passed" ? "ok" : result.status === "skipped" ? "ignored" : "FAILED";
    this.lines.push(`test ${name(test)} ... ${status}`);
  }

  onEnd(_result: FullResult) {
    mkdirSync("test-results", { recursive: true });
    writeFileSync("test-results/cargo.log", this.lines.join("\n") + "\n");
  }
}
