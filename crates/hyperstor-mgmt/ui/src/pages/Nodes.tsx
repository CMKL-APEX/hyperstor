// Storage nodes and their devices: state, last heartbeat; take a device
// out and in, set its weight and class, add and remove devices, drain a
// node (operators). Below, each device's load and drive (D220): the
// matrix of the last minute, and one device's 15 minutes when picked.
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { DeviceMatrix, DevicePanel, useDevicePerf } from "../perf";
import { cluster, nodes as nodeApi } from "../api";
import type { PgMove } from "../gen/hyperstor/v1/admin_pb";
import type { RemoveDeviceResponse } from "../gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, ErrorText, Field, Pill, Space, Table, fmtBytes, input, td, useCan, usePoll } from "../ui";

type Pending =
  | { kind: "device"; device: number; upIn: boolean }
  | { kind: "drain"; node: number; moves: PgMove[] | null }
  | { kind: "weight"; device: number; weight: number }
  | { kind: "class"; device: number; class: string }
  | { kind: "add" }
  | { kind: "remove"; device: number; plan: RemoveDeviceResponse; force: boolean };

/** PG moves as `pool.pg: [before] → [after]`, the first 200. */
function Moves({ moves }: { moves: PgMove[] }) {
  return (
    <ul className="text-xs text-slate-600 max-h-48 overflow-auto font-mono">
      {moves.slice(0, 200).map((m) => (
        <li key={`${m.pool}.${m.pg}`}>
          {m.pool}.{m.pg}: [{m.before.join(",")}] → [{m.after.join(",")}]
        </li>
      ))}
    </ul>
  );
}

export default function Nodes() {
  const can = useCan();
  const status = usePoll(() => cluster.getStatus({}), 2000);
  const [pending, setPending] = useState<Pending | null>(null);
  const [error, setError] = useState<unknown>();
  const [result, setResult] = useState("");
  // The forms' fields.
  const [value, setValue] = useState("");
  const [node, setNode] = useState("");
  const [cls, setCls] = useState("");
  // A removal refused for leaving a pool short, which force allows.
  const [forceable, setForceable] = useState<number | null>(null);

  const done = (ok: boolean) => {
    setPending(null);
    setValue("");
    setNode("");
    setCls("");
    if (ok) status.reload();
  };

  // A removal's plan (D175, D186): what moves, pools left short, the
  // space after; refused plans say why at once.
  const planRemoval = async (device: number, force: boolean) => {
    setError(undefined);
    setForceable(null);
    try {
      const plan = await cluster.removeDevice({ device, dryRun: true, force });
      setPending({ kind: "remove", device, plan, force });
    } catch (e) {
      setError(e);
      if (!force && String((e as Error).message ?? e).includes("--force allows it")) setForceable(device);
    }
  };

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
    <>
    <Card
      title="Nodes and devices"
      actions={can("operate") && <Button onClick={() => setPending({ kind: "add" })}>Add device</Button>}
    >
      <ErrorText error={status.error ?? error} />
      {forceable != null && (
        <p className="mb-2">
          <Button onClick={() => planRemoval(forceable, true)}>Remove device {forceable} anyway (force)</Button>
        </p>
      )}
      {result && <p className="text-sm text-emerald-700 mb-2">{result}</p>}
      <Table head={["Node", "Rack", "State", "Heard", "Device", "Address", "Class", "Weight", "Space", "Placement", ""]}>
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
                <Space total={d.totalBytes} free={d.freeBytes} testId={`device-${d.id}-space`} />
              </td>
              <td className={td}>
                <Pill tone={d.upIn ? "ok" : "warn"}>{d.upIn ? "in" : "out"}</Pill>
              </td>
              <td className={`${td} flex gap-2`}>
                {can("operate") && (
                  <>
                    <Button onClick={() => setPending({ kind: "device", device: d.id, upIn: !d.upIn })}>
                      {d.upIn ? "Take out" : "Bring in"}
                    </Button>
                    <Button
                      onClick={() => {
                        setValue(d.weight.toFixed(2));
                        setPending({ kind: "weight", device: d.id, weight: d.weight });
                      }}
                    >
                      Weight
                    </Button>
                    <Button
                      onClick={() => {
                        setCls(d.class);
                        setPending({ kind: "class", device: d.id, class: d.class });
                      }}
                    >
                      Class
                    </Button>
                    <Button kind="danger" onClick={() => planRemoval(d.id, false)}>
                      Remove
                    </Button>
                  </>
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
          onClose={done}
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
          onClose={done}
        >
          <p className="text-sm">
            Every device of node {pending.node} goes out; {pending.moves?.length ?? 0} PG(s) move. Follow it under
            Operations; cancelling brings the devices back in.
          </p>
          <Moves moves={pending.moves ?? []} />
        </ActionDialog>
      )}
      {pending?.kind === "weight" && (
        <ActionDialog
          title={`Device ${pending.device}'s weight`}
          submit="Set weight"
          cli={`weight --device ${pending.device} --weight ${value.trim() || "auto"}`}
          run={() => {
            const auto = value.trim() === "" || value.trim() === "auto";
            const w = Number(value);
            if (!auto && !(w >= 0)) throw new Error("A weight is a number, 0 or more, or auto");
            return cluster.setDeviceWeight({ device: pending.device, weight: auto ? undefined : w });
          }}
          onClose={done}
        >
          <Field label="Weight (TiB it should take; auto: its capacity)">
            <input className={input} value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
          <p className="text-sm">Its PGs move as the weight changes (D158).</p>
        </ActionDialog>
      )}
      {pending?.kind === "class" && (
        <ActionDialog
          title={`Device ${pending.device}'s class`}
          submit="Set class"
          cli={`device class --device ${pending.device} --class ${cls.trim() || "auto"}`}
          run={() => {
            const auto = cls.trim() === "" || cls.trim() === "auto";
            return cluster.setDeviceClass({ device: pending.device, class: auto ? undefined : cls.trim() });
          }}
          onClose={done}
        >
          <Field label="Class (auto: its kind, nvme, ssd, hdd or file)">
            <input className={input} value={cls} onChange={(e) => setCls(e.target.value)} />
          </Field>
          <p className="text-sm">Pools limited to classes move its PGs (D178).</p>
        </ActionDialog>
      )}
      {pending?.kind === "add" && (
        <ActionDialog
          title="Add a device"
          submit="Add"
          cli={`device add --node ${node || "N"} --address ${value || "A"}${cls ? ` --class ${cls}` : ""}`}
          run={async () => {
            if (!/^\d+$/.test(node)) throw new Error("Pick the node");
            const r = await cluster.addDevice({ node: Number(node), address: value.trim(), class: cls.trim() || undefined });
            setResult(`Device ${r.device} added to node ${node}, at weight 0: restart the node with its shard.`);
          }}
          onClose={done}
        >
          <Field label="Node">
            <select className={input} value={node} onChange={(e) => setNode(e.target.value)}>
              <option value="">–</option>
              {(status.data?.nodes ?? []).map((n) => (
                <option key={n.id} value={n.id}>
                  {n.id}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Address (host:port it serves at)">
            <input className={input} value={value} onChange={(e) => setValue(e.target.value)} />
          </Field>
          <Field label="Class (optional; default: as its other devices)">
            <input className={input} value={cls} onChange={(e) => setCls(e.target.value)} />
          </Field>
          <p className="text-sm">
            It joins at weight 0, so nothing moves until its node restarts with one more shard and it reports its
            capacity (D167).
          </p>
        </ActionDialog>
      )}
      {pending?.kind === "remove" && (
        <ActionDialog
          title={`Remove device ${pending.device}`}
          submit="Remove"
          kind="danger"
          cli={`device remove --device ${pending.device}${pending.force ? " --force" : ""}`}
          run={async () => {
            const r = await cluster.removeDevice({ device: pending.device, dryRun: false, force: pending.force });
            setResult(`Removing device ${pending.device}: operation ${r.operation?.id ?? ""} (Operations).`);
          }}
          onClose={done}
        >
          <p className="text-sm">
            It goes out, {pending.plan.moves.length} PG(s) move to the other devices of their pools, then it leaves the
            map, with its node if it is the last; neither id is given again (D186). Its data is left as it is. This
            needs a login of the last 10 minutes.
          </p>
          <Moves moves={pending.plan.moves} />
          {pending.plan.shortPools.length > 0 && (
            <p className="text-sm text-amber-700" data-testid="remove-short">
              Left short:{" "}
              {pending.plan.shortPools.map((s) => `pool ${s.pool} keeps ${s.copies} of ${s.size} copies`).join("; ")}
            </p>
          )}
          {pending.plan.fill.length > 0 && (
            <ul className="text-xs text-slate-600">
              {pending.plan.fill.map((f) => (
                <li key={f.device}>
                  device {f.device}: {fmtBytes(f.usedBytes)} of {fmtBytes(f.totalBytes)} after
                </li>
              ))}
            </ul>
          )}
        </ActionDialog>
      )}
    </Card>
    <DevicePerfCard />
    </>
  );
}

/** Every device's last minute; a picked one's panel (also from the
 * Advisor's links, `?device=D`). */
function DevicePerfCard() {
  const [params, setParams] = useSearchParams();
  const picked = params.get("device") == null ? null : Number(params.get("device"));
  const { data, error } = useDevicePerf();
  const pick = (d: number | null) => setParams(d == null ? {} : { device: String(d) });
  return (
    <>
      <Card title="Device performance (last minute)">
        <ErrorText error={error} />
        <DeviceMatrix devices={data ?? []} picked={picked} onPick={(d) => pick(picked === d ? null : d)} />
        <p className="text-xs text-slate-500 mt-2">
          Requests its PGs served as primary; read and commit p99 are the device's own time. Click a device for its drive and 15 minutes; longer history is in Grafana.
        </p>
      </Card>
      {picked != null && <DevicePanel device={picked} onClose={() => pick(null)} />}
    </>
  );
}
