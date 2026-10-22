// Pools with their PG states, and one pool's PGs with their acting and up
// sets.
import { useState } from "react";
import { cluster } from "../api";
import { Card, Cli, ErrorText, Table, td, usePoll } from "../ui";

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
  const pgs = usePoll(async () => (pool == null ? null : (await cluster.listPgs({ pool })).pgs), pool == null ? 0 : 5000);
  return (
    <>
      <Card title="Pools">
        <ErrorText error={status.error} />
        <Table head={["Pool", "Kind", "Copies", "Domain", "Chunk", "PGs", "States", "Clean", "Degraded", "Backfilling", "Inactive", ""]}>
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
                  <button className="text-sky-700 dark:text-sky-300 hover:underline" onClick={() => setPool(p.id)}>
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
        <Card title={`Pool ${pool}'s PGs`}>
          <ErrorText error={pgs.error} />
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
          <Cli cmd={`pgs --pool ${pool}`} />
        </Card>
      )}
    </>
  );
}
