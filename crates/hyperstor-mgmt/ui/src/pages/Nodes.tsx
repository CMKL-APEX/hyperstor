// Storage nodes and their devices: state, last heartbeat; take a device
// out and in, drain a node (operators).
import { useState } from "react";
import { cluster, nodes as nodeApi } from "../api";
import type { PgMove } from "../gen/hyperstor/v1/admin_pb";
import { ActionDialog, Button, Card, ErrorText, Pill, Table, td, useCan, usePoll } from "../ui";

type Pending =
  | { kind: "device"; device: number; upIn: boolean }
  | { kind: "drain"; node: number; moves: PgMove[] | null };

export default function Nodes() {
  const can = useCan();
  const status = usePoll(() => cluster.getStatus({}), 2000);
  const [pending, setPending] = useState<Pending | null>(null);
  const [error, setError] = useState<unknown>();
  const [result, setResult] = useState("");

  const preview = async (node: number) => {
    setError(undefined);
    try {
      const r = await nodeApi.drainNode({ node, dryRun: true });
      setPending({ kind: "drain", node, moves: r.moves });
    } catch (e) {
      setError(e);
    }
  };

  return (
    <Card title="Nodes and devices">
      <ErrorText error={status.error ?? error} />
      {result && <p className="text-sm text-emerald-700 mb-2">{result}</p>}
      <Table head={["Node", "Rack", "State", "Heard", "Device", "Address", "Class", "Weight", "Placement", ""]}>
        {(status.data?.nodes ?? []).flatMap((n) =>
          n.devices.map((d, i) => (
            <tr key={`${n.id}-${d.id}`} data-testid={`device-${d.id}`}>
              <td className={td}>{i === 0 ? n.id : ""}</td>
              <td className={td}>{i === 0 ? n.rack : ""}</td>
              <td className={td}>{i === 0 && <Pill tone={n.up ? "ok" : "error"}>{n.up ? "up" : "down"}</Pill>}</td>
              <td className={`${td} text-slate-500`}>
                {i === 0 && (n.lastHeardMs == null ? "never" : `${(Number(n.lastHeardMs) / 1000).toFixed(1)} s ago`)}
              </td>
              <td className={td}>{d.id}</td>
              <td className={td}>{d.addr}</td>
              <td className={td}>{d.class || "–"}</td>
              <td className={td}>{d.weight.toFixed(2)}</td>
              <td className={td}>
                <Pill tone={d.upIn ? "ok" : "warn"}>{d.upIn ? "in" : "out"}</Pill>
              </td>
              <td className={`${td} flex gap-2`}>
                {can("operate") && (
                  <Button onClick={() => setPending({ kind: "device", device: d.id, upIn: !d.upIn })}>
                    {d.upIn ? "Take out" : "Bring in"}
                  </Button>
                )}
                {can("operate") && i === 0 && <Button onClick={() => preview(n.id)}>Drain node</Button>}
              </td>
            </tr>
          )),
        )}
      </Table>
      {pending?.kind === "device" && (
        <ActionDialog
          title={pending.upIn ? `Bring device ${pending.device} in` : `Take device ${pending.device} out`}
          submit={pending.upIn ? "Bring in" : "Take out"}
          kind={pending.upIn ? "primary" : "danger"}
          cli={`${pending.upIn ? "in" : "out"} --device ${pending.device}`}
          run={() => cluster.setDeviceIn({ device: pending.device, upIn: pending.upIn })}
          onClose={(done) => {
            setPending(null);
            if (done) status.reload();
          }}
        >
          <p className="text-sm">
            {pending.upIn
              ? "Its PGs come back to it, copied from the others."
              : "Its PGs move to other devices; it can then be serviced. Until they have, they have a copy fewer."}
          </p>
        </ActionDialog>
      )}
      {pending?.kind === "drain" && (
        <ActionDialog
          title={`Drain node ${pending.node}`}
          submit="Drain"
          kind="danger"
          cli={`drain --node ${pending.node}`}
          run={async () => {
            const r = await nodeApi.drainNode({ node: pending.node, dryRun: false });
            setResult(`Draining node ${pending.node}: operation ${r.operation?.id ?? ""} (Operations).`);
          }}
          onClose={(done) => {
            setPending(null);
            if (done) status.reload();
          }}
        >
          <p className="text-sm">
            Every device of node {pending.node} goes out; {pending.moves?.length ?? 0} PG(s) move. Follow it under
            Operations; cancelling brings the devices back in.
          </p>
          <ul className="text-xs text-slate-600 max-h-48 overflow-auto font-mono">
            {(pending.moves ?? []).slice(0, 200).map((m) => (
              <li key={`${m.pool}.${m.pg}`}>
                {m.pool}.{m.pg}: [{m.before.join(",")}] → [{m.after.join(",")}]
              </li>
            ))}
          </ul>
        </ActionDialog>
      )}
    </Card>
  );
}
