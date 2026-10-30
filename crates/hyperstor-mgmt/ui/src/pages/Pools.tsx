// Pools with their PG states, and one pool's PGs as a grid (D154): a
// PG's acting and up sets when it is picked, the whole table on request.
import { useEffect, useState } from "react";
import { cluster } from "../api";
import { PgGrid } from "../PgGrid";
import { Button, Card, Cli, ErrorText, Space, Table, td, usePoll } from "../ui";

const STATES = [
  ["clean", "bg-emerald-500"],
  ["degraded", "bg-amber-500"],
  ["backfilling", "bg-sky-500"],
  ["inactive", "bg-red-500"],
] as const;

const NONE = 4294967295;
const set = (v: number[]) => `[${v.map((d) => (d === NONE ? "-" : d)).join(",")}]`;

export default function Pools() {
  const status = usePoll(() => cluster.getStatus({}), 2000);
  const [pool, setPool] = useState<number | null>(null);
  const [pg, setPg] = useState<number | null>(null);
  const [all, setAll] = useState(false);
  // Loaded when asked for, not polled: a pool may have many PGs.
  const pgs = usePoll(
    async () => (pool == null || (pg == null && !all) ? null : (await cluster.listPgs({ pool, pg: all ? undefined : pg! })).pgs),
    0,
  );
  const { reload } = pgs;
  useEffect(() => {
    reload();
  }, [reload, pool, pg, all]);
  const open = (p: number) => {
    setPool(p);
    setPg(null);
    setAll(false);
  };
  return (
    <>
      <Card title="Pools">
        <ErrorText error={status.error} />
        <Table head={["Pool", "Kind", "Copies", "Domain", "Chunk", "PGs", "Space", "States", "Clean", "Degraded", "Backfilling", "Inactive", ""]}>
          {(status.data?.pools ?? []).map((p) => {
            const c = p.pgs;
            const total = p.pgCount || 1;
            return (
              <tr key={p.id} data-testid={`pool-${p.id}`}>
                <td className={td}>
                  {p.id} {p.name}
                </td>
                <td className={td}>{p.kind}</td>
                <td className={td}>
                  {p.size}/{p.minSize}
                </td>
                <td className={td}>{p.domain}</td>
                <td className={td}>{p.chunkSize ? `${p.chunkSize / 1024}K` : "–"}</td>
                <td className={td}>{p.pgCount}</td>
                <td className={td}>
                  <Space total={p.totalBytes} free={p.freeBytes} testId={`pool-${p.id}-space`} />
                </td>
                <td className={td}>
                  <div className="flex h-2.5 w-40 rounded overflow-hidden bg-slate-200 dark:bg-slate-700">
                    {STATES.map(([k, color]) => {
                      const n = c?.[k] ?? 0;
                      return n ? <span key={k} className={color} style={{ width: `${(100 * n) / total}%` }} /> : null;
                    })}
                  </div>
                </td>
                <td className={td}>{c?.clean ?? 0}</td>
                <td className={td}>{c?.degraded ?? 0}</td>
                <td className={td}>{c?.backfilling ?? 0}</td>
                <td className={td}>{c?.inactive ?? 0}</td>
                <td className={td}>
                  <button className="text-sky-700 dark:text-sky-300 hover:underline" onClick={() => open(p.id)}>
                    PGs
                  </button>
                </td>
              </tr>
            );
          })}
        </Table>
        <Cli cmd="pools" />
      </Card>
      {pool != null && (
        <Card
          title={`Pool ${pool}'s PGs`}
          actions={!all && <Button onClick={() => setAll(true)}>List all</Button>}
        >
          <PgGrid key={pool} pool={pool} max={16} picked={all ? null : pg} onPick={(i) => (setAll(false), setPg(i))} />
          <ErrorText error={pgs.error} />
          {(pg != null || all) && (
            <Table head={["PG", "Epoch", "Acting", "Up"]}>
              {(pgs.data ?? []).map((g) => (
                <tr key={g.pg}>
                  <td className={td}>
                    {pool}.{g.pg}
                  </td>
                  <td className={td}>{String(g.epoch)}</td>
                  <td className={`${td} font-mono`}>{set(g.acting)}</td>
                  <td className={`${td} font-mono`}>{set(g.up)}</td>
                </tr>
              ))}
            </Table>
          )}
          <Cli cmd={pg != null && !all ? `pgs --pool ${pool} --pg ${pg}` : `pg-grid --pool ${pool}`} />
        </Card>
      )}
    </>
  );
}
