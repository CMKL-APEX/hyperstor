// A pool's scrub (28-scrub.md §31.8): its settings, edited in place
// (weakening them takes a reason and a fresh login, D226), how recently
// its PGs were scrubbed, and "Scrub now" (D229). The PG grid's Scrub view
// shades each PG by the age of its last deep scrub, from these records.
import { useEffect, useState } from "react";
import { ApiError, cluster } from "./api";
import type { PgScrubReport, PgScrubs, PoolScrub } from "./gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, input, td, useCan, usePoll } from "./ui";

/** Seconds as the CLI shows them: 14s, 5m, 10h, 3d. */
export function fmtAge(secs: number | bigint) {
  const s = Number(secs);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 2 * 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** Fresh to due (emerald-500 to amber-400), then overdue (red-500), for
 * an age `r` in deep intervals: due at 1, overdue at 2 (D228). */
export function ageColor(r: number) {
  if (r >= 2) return "#ef4444";
  const a = [16, 185, 129];
  const b = [251, 191, 36];
  const x = Math.min(1, Math.max(0, r));
  const c = a.map((v, i) => Math.round(v + (b[i] - v) * x));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}

/** Whether a report left something to look at. */
export const found = (r?: PgScrubReport) => !!r && r.inconsistent + r.unresolved + r.damaged > 0;

/** A pool's PG records and settings, every 10 s while `on`. */
export function usePgScrubs(pool: number | null, on: boolean) {
  const r = usePoll(async () => {
    if (pool == null || !on) return null;
    const [pgs, status] = await Promise.all([cluster.listPgScrubs({ pool }), cluster.getScrubStatus({ pool })]);
    return { pgs: pgs.pgs, pool: status.pools[0] as PoolScrub | undefined, now: Math.floor(Date.now() / 1000) };
  }, on ? 10000 : 0);
  const { reload } = r;
  useEffect(() => {
    reload();
  }, [reload, pool, on]);
  return r;
}

export type PgScrubData = NonNullable<ReturnType<typeof usePgScrubs>["data"]>;

/** Seconds between deep scrubs of a PG under `p`'s settings; 0 if off. */
export const deepSecs = (p?: PoolScrub) => (p?.settings ? p.settings.hours * p.settings.deepEvery * 3600 : 0);

/** How old a PG's last deep scrub is, in deep intervals. */
export function deepAge(g: PgScrubs, d: PgScrubData) {
  const every = deepSecs(d.pool);
  const at = Number(g.deep?.at ?? g.since);
  return every ? Math.max(0, d.now - at) / every : 0;
}

function Report({ what, r }: { what: string; r?: PgScrubReport }) {
  if (!r)
    return (
      <tr>
        <td className={td}>{what}</td>
        <td className={td} colSpan={6}>
          none yet
        </td>
      </tr>
    );
  return (
    <tr>
      <td className={td}>{what}</td>
      <td className={td}>{fmtAge(Math.max(0, Math.floor(Date.now() / 1000) - Number(r.at)))} ago</td>
      <td className={td}>{(r.tookMs / 1000).toFixed(1)} s</td>
      <td className={td}>{r.compared}</td>
      <td className={td}>
        {r.inconsistent} / {r.repaired} / {r.unresolved}
      </td>
      <td className={td}>{r.damaged}</td>
      <td className={td}>{r.holesSkipped ? `skipped: ${r.holesSkipped}` : `${r.holes} (${r.holesRepaired} repaired)`}</td>
    </tr>
  );
}

/** One PG's last light and deep scrub, for the grid's detail. */
export function PgScrubDetail({ pool, g }: { pool: number; g: PgScrubs }) {
  return (
    <div className="text-sm grid gap-1" data-testid="pg-scrub">
      <p>
        PG {pool}.{g.pg}
        {g.overdue && (
          <>
            {" "}
            <Pill tone="warn">{g.overdue} scrub overdue</Pill>
          </>
        )}
        {g.skipped >= 2 && (
          <>
            {" "}
            <Pill tone="warn">holes unchecked twice</Pill>
          </>
        )}
      </p>
      <Table head={["Scrub", "When", "Took", "Compared", "Inconsistent / repaired / unresolved", "Damaged", "Holes"]}>
        <Report what="light" r={g.light} />
        <Report what="deep" r={g.deep} />
      </Table>
    </div>
  );
}

/** A pool's Scrub card. */
export function ScrubCard({ pool }: { pool: number }) {
  const can = useCan();
  const s = usePoll(async () => (await cluster.getScrubStatus({ pool })).pools[0] ?? null, 10000);
  const { reload } = s;
  useEffect(() => {
    reload();
  }, [reload, pool]);
  const [editing, setEditing] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState<unknown>();
  const [hours, setHours] = useState("");
  const [deepEvery, setDeepEvery] = useState("");
  const [holes, setHoles] = useState("repair");
  const [rate, setRate] = useState("");
  const [reason, setReason] = useState("");
  const p = s.data;
  // A cache pool isn't scrubbed, and isn't listed.
  if (s.data === null) return null;
  const set = p?.settings;
  const edit = () => {
    if (!set) return;
    setHours(String(set.hours));
    setDeepEvery(String(set.deepEvery));
    setHoles(set.holeRepair || "repair");
    setRate(String(set.deepRateMib));
    setReason("");
    setEditing(true);
  };
  const changed = () => {
    const n = (v: string, was: number, what: string) => {
      if (v.trim() === String(was)) return undefined;
      const x = Number(v);
      if (!Number.isInteger(x) || x < 0) throw new Error(`${what} is a whole number`);
      return x;
    };
    return {
      pool,
      hours: n(hours, set!.hours, "Hours"),
      deepEvery: n(deepEvery, set!.deepEvery, "Deep every"),
      holeRepair: holes !== set!.holeRepair ? holes : undefined,
      deepRateMib: n(rate, set!.deepRateMib, "The deep rate"),
      reason: reason.trim(),
    };
  };
  const cli = () => {
    if (!set) return "";
    return [
      `scrub set --pool ${pool}`,
      hours !== String(set.hours) && `--hours ${hours}`,
      deepEvery !== String(set.deepEvery) && `--deep-every ${deepEvery}`,
      holes !== set.holeRepair && `--hole-repair ${holes}`,
      rate !== String(set.deepRateMib) && `--deep-rate ${rate}`,
      reason.trim() && `--reason '${reason.trim().replace(/'/g, "'\\''")}'`,
    ]
      .filter(Boolean)
      .join(" ");
  };
  const start = async (deep: boolean) => {
    setError(undefined);
    setNotice("");
    try {
      const r = await cluster.startScrub({ pool, deep });
      setNotice(`Scrubbing ${r.operation?.total ?? 0} PG(s)${deep ? " deep" : ""}: operation ${r.operation?.id} (Operations).`);
    } catch (e) {
      setError(e);
    }
  };
  return (
    <Card
      title={`Pool ${pool}'s scrub`}
      actions={
        can("store") &&
        set && (
          <>
            <Button onClick={() => start(false)}>Scrub now</Button>
            <Button onClick={() => start(true)}>Scrub deep now</Button>
            <Button onClick={edit}>Change</Button>
          </>
        )
      }
    >
      <ErrorText error={s.error ?? error} />
      {notice && <p className="text-sm text-emerald-700 mb-2">{notice}</p>}
      {p && set && (
        <div className="text-sm grid gap-2" data-testid={`scrub-${pool}`}>
          <p>
            {set.hours ? (
              <>
                Every PG light-scrubbed every {set.hours} h, and deep every {set.deepEvery} scrubs (
                {fmtAge(deepSecs(p))}), reading at most {set.deepRateMib} MiB/s per device; holes{" "}
                {set.holeRepair === "report" ? "only reported" : "repaired"}.
              </>
            ) : (
              <>Scrub is off.</>
            )}
            {set.weakened.length > 0 && (
              <>
                {" "}
                <Pill tone="warn">reduced: {set.weakened.join(", ")}</Pill>
              </>
            )}
          </p>
          <ul className="flex flex-wrap gap-x-6 gap-y-1">
            <li data-testid={`scrub-${pool}-light`}>Oldest light scrub: {fmtAge(p.oldestLightSecs)} ago</li>
            <li data-testid={`scrub-${pool}-deep`}>Oldest deep scrub: {fmtAge(p.oldestDeepSecs)} ago</li>
            <li data-testid={`scrub-${pool}-overdue`}>
              Overdue: {p.overdue}
              {p.overdue > 0 && ` (${p.overduePgs.join(", ")}${p.overdue > p.overduePgs.length ? ", …" : ""})`}
            </li>
            <li>Holes unchecked: {p.unchecked}</li>
            <li className={p.damaged ? "text-red-700 dark:text-red-400" : undefined}>Damaged: {p.damaged}</li>
            <li>Inconsistent: {p.inconsistent}</li>
          </ul>
          {p.week && (
            <p className="text-slate-600 dark:text-slate-400">
              Last 7 days: {p.week.scrubs} scrubs, {String(p.week.compared)} objects compared, {String(p.week.inconsistent)}{" "}
              inconsistent, {String(p.week.repaired)} repaired, {String(p.week.damaged)} damaged, {String(p.week.holes)} with holes
              ({String(p.week.holesRepaired)} repaired).
            </p>
          )}
        </div>
      )}
      <Cli cmd={`scrub status --pool ${pool}`} />
      {editing && set && (
        <ActionDialog
          title={`Pool ${pool}'s scrub`}
          submit="Save"
          cli={cli()}
          run={async () => {
            const req = changed();
            try {
              if (req.reason) await cluster.weakenScrubSettings(req);
              else await cluster.setScrubSettings(req);
            } catch (e) {
              if (e instanceof ApiError && e.code === "weakens_scrub")
                throw new Error(`${e.message}. Give a reason to weaken it; it asks for a fresh login.`);
              throw e;
            }
            setNotice(`Pool ${pool}'s scrub settings changed.`);
          }}
          onClose={(done) => {
            setEditing(false);
            if (done) reload();
          }}
        >
          <div className="grid grid-cols-2 gap-2">
            <Field label="Light scrub every (hours, 1–720; 0: off)">
              <input className={input} value={hours} onChange={(e) => setHours(e.target.value)} />
            </Field>
            <Field label="Deep every (scrubs, 1–28)">
              <input className={input} value={deepEvery} onChange={(e) => setDeepEvery(e.target.value)} />
            </Field>
            <Field label="Holes">
              <select className={input} value={holes} onChange={(e) => setHoles(e.target.value)}>
                <option value="repair">repaired</option>
                <option value="report">only reported</option>
              </select>
            </Field>
            <Field label="Deep rate (MiB/s per device, 8–1024)">
              <input className={input} value={rate} onChange={(e) => setRate(e.target.value)} />
            </Field>
          </div>
          <Field label="Reason (needed to check less than the defaults)">
            <input className={input} value={reason} onChange={(e) => setReason(e.target.value)} />
          </Field>
          <p className="text-sm">
            Defaults: every 24 h, deep every 7, holes repaired, 64 MiB/s. Scrub off, more than a week, deep less often than every
            7, holes only reported or under 32 MiB/s checks less: that needs a reason, kept in the audit log, and a login of the
            last 10 minutes, and health warns while it lasts (D226).
          </p>
        </ActionDialog>
      )}
    </Card>
  );
}
