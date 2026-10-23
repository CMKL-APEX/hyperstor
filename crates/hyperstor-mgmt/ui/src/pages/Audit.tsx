// The audit log (D112, D113): every change and refused call, newest
// first, for security administrators; filter, page on, export, and how
// long it is kept.
import { useState } from "react";
import { audit } from "../api";
import type { AuditRecord } from "../gen/hyperstor/v1/admin_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Table, input, td, useCan, usePoll } from "../ui";

const SINCE = [
  ["", "any time"],
  ["3600", "the last hour"],
  ["86400", "the last day"],
  ["604800", "the last week"],
] as const;

function fmtMs(ms: bigint) {
  return new Date(Number(ms)).toLocaleString();
}

export default function Audit() {
  const can = useCan();
  const [principal, setPrincipal] = useState("");
  const [op, setOp] = useState("");
  const [since, setSince] = useState("");
  const [records, setRecords] = useState<AuditRecord[]>([]);
  const [next, setNext] = useState("");
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  const settings = usePoll(async () => (can("secure") ? (await audit.getAuditSettings({})).settings : undefined), 0);
  const [editing, setEditing] = useState(false);
  const [days, setDays] = useState("");

  const load = async (after: string) => {
    setBusy(true);
    setError(undefined);
    try {
      const now = BigInt(Math.floor(Date.now() / 1000));
      const r = await audit.listAudit({
        principal,
        op,
        sinceUnix: since ? now - BigInt(since) : 0n,
        limit: 100,
        after,
      });
      setRecords((x) => (after ? [...x, ...r.records] : r.records));
      setNext(r.next);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const exportLines = () => {
    const text = records
      .map((r) =>
        JSON.stringify({
          audit: 1,
          time: Number(r.timeMs) / 1000,
          principal: r.principal,
          op: r.op,
          target: r.target,
          result: r.result,
        }),
      )
      .join("\n");
    const url = URL.createObjectURL(new Blob([text + "\n"], { type: "application/x-ndjson" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = "hyperstor-audit.jsonl";
    a.click();
    URL.revokeObjectURL(url);
  };

  if (!can("secure")) {
    return (
      <Card title="Audit">
        <p className="text-sm text-slate-600">The audit log is for security administrators.</p>
      </Card>
    );
  }

  const cli = ["audit list", principal && `--principal ${principal}`, op && `--op ${op}`, since && `--since ${Number(since) / 3600}h`]
    .filter(Boolean)
    .join(" ");

  return (
    <>
      <Card
        title="Audit log"
        actions={
          <>
            <Button onClick={exportLines} disabled={!records.length}>
              Export
            </Button>
          </>
        }
      >
        <form
          className="flex flex-wrap items-end gap-3 mb-3"
          onSubmit={(e) => {
            e.preventDefault();
            load("");
          }}
        >
          <Field label="Who (contains)">
            <input className={input} value={principal} onChange={(e) => setPrincipal(e.target.value)} placeholder="user:ann" />
          </Field>
          <Field label="Operation">
            <input className={input} value={op} onChange={(e) => setOp(e.target.value)} placeholder="CreateVolume" />
          </Field>
          <Field label="When">
            <select className={input} value={since} onChange={(e) => setSince(e.target.value)} aria-label="When">
              {SINCE.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
          </Field>
          <Button type="submit" kind="primary" disabled={busy}>
            Show
          </Button>
        </form>
        <ErrorText error={error} />
        <Table head={["When", "Who", "Operation", "Target", "Result"]} empty="No records shown yet: choose a filter and Show.">
          {records.map((r, i) => (
            <tr key={i} data-testid="audit-record">
              <td className={td}>{fmtMs(r.timeMs)}</td>
              <td className={`${td} text-xs`}>{r.principal}</td>
              <td className={td}>{r.op}</td>
              <td className={`${td} text-xs`}>{r.target}</td>
              <td className={`${td} whitespace-normal text-xs`}>{r.result}</td>
            </tr>
          ))}
        </Table>
        {next && (
          <div className="mt-3">
            <Button onClick={() => load(next)} disabled={busy}>
              Older records
            </Button>
          </div>
        )}
        <Cli cmd={cli} />
      </Card>
      <Card title="Retention">
        <div className="flex items-center gap-3 text-sm" data-testid="audit-retention">
          <span>
            {settings.data
              ? settings.data.retentionDays
                ? `Records are kept ${settings.data.retentionDays} days.`
                : "Records are kept forever."
              : "…"}
          </span>
          <Button
            onClick={() => {
              setDays(String(settings.data?.retentionDays ?? 365));
              setEditing(true);
            }}
          >
            Change
          </Button>
        </div>
        {editing && (
          <ActionDialog
            title="Keep audit records"
            submit="Save"
            cli={`audit settings --retention-days ${days || "N"}`}
            run={() => audit.setAuditSettings({ retentionDays: Number(days || "0") })}
            onClose={(done) => {
              setEditing(false);
              if (done) settings.reload();
            }}
          >
            <Field label="Days (0: forever)">
              <input className={input} value={days} onChange={(e) => setDays(e.target.value.replace(/\D/g, ""))} />
            </Field>
            <p className="text-xs text-slate-500">Whole days older than this are dropped. This needs a login of the last 10 minutes.</p>
          </ActionDialog>
        )}
      </Card>
    </>
  );
}
