// The Advisor (27-performance.md §30.5, D222): what the load and the drives
// suggest doing, each finding with the numbers behind it, what to do and
// the command. Nothing is done for the operator; a finding can be snoozed
// for a day, in this browser.
import { Card, Cli } from "../ui";
import { Findings } from "../perf";

export default function Advisor() {
  return (
    <Card title="Advisor">
      <p className="text-sm text-slate-600 dark:text-slate-400 mb-3">
        From the last minute of every device's reports to the map service's leader and what each drive says of itself. The
        rules and their thresholds are in the documentation; nothing here changes anything.
      </p>
      <Findings />
      <Cli cmd="advise" />
    </Card>
  );
}
