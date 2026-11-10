// The cluster's networks (26-networks.md; D284): the data and control
// networks of the map, set and removed by operators; each device's
// endpoints on them (D215, D285) and whether peers reach them (D210); and
// the nodes' ports, their roles (D237) and the traffic on them.
import { useState } from "react";
import { cluster } from "../api";
import type { Device, Network, Port } from "../gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, input, td, useCan, usePoll } from "../ui";

const KINDS = ["data", "control", "both"] as const;
const ROLES = ["storage", "converged", "compute", "host", "uplink", "vm"] as const;

const mbs = (b: bigint) => (Number(b) / 1e6).toFixed(1);

/** Endpoints as `NET=IP:PORT, ...`, as `device endpoints` takes them. */
function endpointsText(d: Device): string {
  return d.endpoints.map((e) => `${e.network}=${e.address}`).join(", ");
}

export default function Networks() {
  const can = useCan();
  const status = usePoll(() => cluster.getStatus({}), 3000);
  const ports = usePoll(() => cluster.listPorts({}), 10000);
  const [edit, setEdit] = useState<Network | "new" | null>(null);
  const [remove, setRemove] = useState<Network | null>(null);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<string>("data");
  const [match, setMatch] = useState("");
  const [trusted, setTrusted] = useState(false);
  const [device, setDevice] = useState<Device | null>(null);
  const [eps, setEps] = useState("");
  const [port, setPort] = useState<Port | null>(null);
  const [role, setRole] = useState("default");
  const nets = status.data?.networks ?? [];
  const devices = (status.data?.nodes ?? []).flatMap((n) => n.devices.map((d) => ({ node: n.id, d })));
  const used = (net: string) => devices.filter(({ d }) => d.endpoints.some((e) => e.network === net)).length;
  const openNet = (n: Network | "new") => {
    setEdit(n);
    setName(n === "new" ? "" : n.name);
    setKind(n === "new" ? "data" : n.kind);
    setMatch(n === "new" ? "" : n.matches);
    setTrusted(n === "new" ? false : n.trusted);
  };
  const parsed = () =>
    eps
      .split(/[,\s]+/)
      .filter(Boolean)
      .map((e) => {
        const [network, address] = e.split("=");
        if (!network || !address) throw new Error(`${e}: not NETWORK=IP:PORT`);
        return { network, address };
      });
  return (
    <>
      <Card
        title="Networks"
        actions={
          can("operate") && (
            <Button kind="primary" onClick={() => openNet("new")}>
              Add network
            </Button>
          )
        }
      >
        <ErrorText error={status.error} />
        <Table head={["Network", "Kind", "Trusted", "Match", "Endpoints on it", ""]} empty="No networks: storage uses devices' addresses.">
          {nets.map((n) => (
            <tr key={n.name} data-testid={`network-${n.name}`}>
              <td className={td}>{n.name}</td>
              <td className={td}>{n.kind}</td>
              <td className={td}>{n.trusted ? "yes" : "no"}</td>
              <td className={td}>{n.matches}</td>
              <td className={td}>{used(n.name)}</td>
              <td className={td}>
                {can("operate") && (
                  <span className="flex gap-2">
                    <Button onClick={() => openNet(n)}>Change</Button>
                    <Button kind="danger" onClick={() => setRemove(n)} disabled={used(n.name) > 0}
                      title={used(n.name) > 0 ? "Devices have endpoints on it" : undefined}>
                      Remove
                    </Button>
                  </span>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          Storage nodes reach each other, and clients reach them, at their devices' endpoints on the data networks they
          share, else at the devices' addresses. A network with endpoints on it keeps carrying data and keeps its match.
          Nothing on the storage path is encrypted yet; trusted marks where it may stay plaintext once it is.
        </p>
        <Cli cmd="network list" />
      </Card>
      <Card title="Endpoints">
        <Table head={["Node", "Device", "Address", "Endpoints", ""]}>
          {devices.map(({ node, d }) => (
            <tr key={d.id} data-testid={`endpoints-${d.id}`}>
              <td className={td}>{node}</td>
              <td className={td}>{d.id}</td>
              <td className={td}>{d.addr}</td>
              <td className={td}>
                <span className="flex flex-wrap gap-1">
                  {d.endpoints.length === 0 && <span className="text-slate-400">none</span>}
                  {d.endpoints.map((e) => (
                    <Pill key={`${e.network}-${e.address}`} tone={e.down ? "error" : "ok"}>
                      {e.network} {e.address}
                      {e.down ? " down" : ""}
                    </Pill>
                  ))}
                </span>
              </td>
              <td className={td}>
                {can("operate") && nets.length > 0 && (
                  <Button
                    onClick={() => {
                      setDevice(d);
                      setEps(endpointsText(d));
                    }}
                  >
                    Endpoints
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          A host that joined with a token proposes its devices' endpoints at each installer run; on a map service host, or
          one installed with <code>--endpoints off</code>, set them here and run the installer there, which restarts the
          node with them. <em>down</em>: two peers found it silent, so traffic takes the device's other paths.
        </p>
      </Card>
      <Card title="Ports">
        <ErrorText error={ports.error} />
        <Table head={["Node", "Port", "Role", "State", "Gbit/s", "Tx MB/s", "Rx MB/s", "Other", "Weight", "Addresses", ""]}>
          {(ports.data?.ports ?? []).map((p) => (
            <tr key={`${p.node}/${p.name}`} data-testid={`port-${p.node}-${p.name}`}>
              <td className={td}>{p.node}</td>
              <td className={td}>{p.name}</td>
              <td className={td}>
                {p.role}
                {p.roleSet ? " (set)" : ""}
                {p.lent ? " · lent" : ""}
              </td>
              <td className={td}>
                <Pill tone={!p.reported ? "muted" : p.up ? "ok" : "error"}>{!p.reported ? "–" : p.up ? "up" : "down"}</Pill>
              </td>
              <td className={td}>{Math.round(p.speedMbps / 1000)}</td>
              <td className={td}>{mbs(p.txBytesPerSec)}</td>
              <td className={td}>{mbs(p.rxBytesPerSec)}</td>
              <td className={td}>{mbs(p.otherBytesPerSec)}</td>
              <td className={td}>{p.weightMbs ? `${(p.weightMbs / 1000).toFixed(1)}G` : "–"}</td>
              <td className={td}>{p.addresses.join(", ")}</td>
              <td className={td}>
                {can("operate") && (
                  <Button
                    onClick={() => {
                      setPort(p);
                      setRole(p.roleSet ? p.role : "default");
                    }}
                  >
                    Role
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          From each node's reports (every 10 s) to the leader. Storage uses <em>storage</em> and <em>converged</em> ports,
          and a <em>compute</em> port only while it is idle and lent; a port with endpoints is <em>storage</em> unless set.
        </p>
        <Cli cmd="port list" />
      </Card>
      {edit && (
        <ActionDialog
          title={edit === "new" ? "Add a network" : `Change network ${edit.name}`}
          submit={edit === "new" ? "Add" : "Change"}
          cli={`network set --name ${name || "NAME"} --kind ${kind} --match "${match || "subnet IP/LEN"}"${trusted ? " --trusted" : ""}`}
          run={() => cluster.setNetwork({ network: { name, kind, trusted, matches: match } })}
          onClose={(done) => {
            setEdit(null);
            if (done) status.reload();
          }}
        >
          <Field label="Name (lowercase letters, digits, dashes)">
            <input className={input} value={name} disabled={edit !== "new"} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Kind">
            <select className={input} value={kind} onChange={(e) => setKind(e.target.value)}>
              {KINDS.map((k) => (
                <option key={k}>{k}</option>
              ))}
            </select>
          </Field>
          <Field label="Match: subnet IP/LEN, or ib PREFIX (four groups)">
            <input className={input} value={match} placeholder="subnet 192.0.2.0/24" onChange={(e) => setMatch(e.target.value)} />
          </Field>
          <label className="text-sm flex gap-2 items-center">
            <input type="checkbox" checked={trusted} onChange={(e) => setTrusted(e.target.checked)} />
            Trusted: plaintext after the handshake on it
          </label>
          <p className="text-xs text-slate-500">Hosts take endpoints on it at their installer's next run.</p>
        </ActionDialog>
      )}
      {remove && (
        <ActionDialog
          title={`Remove network ${remove.name}`}
          submit="Remove"
          kind="danger"
          cli={`network remove --name ${remove.name}`}
          run={() => cluster.setNetwork({ network: { name: remove.name }, remove: true })}
          onClose={(done) => {
            setRemove(null);
            if (done) status.reload();
          }}
        />
      )}
      {device && (
        <ActionDialog
          title={`Endpoints of device ${device.id}`}
          submit="Set"
          cli={`device endpoints --device ${device.id} --endpoints ${eps.replace(/\s+/g, "") || "none"}`}
          run={() => cluster.setEndpoints({ device: device.id, endpoints: parsed() })}
          onClose={(done) => {
            setDevice(null);
            if (done) status.reload();
          }}
        >
          <Field label="NETWORK=IP:PORT, comma-separated; empty for none">
            <input className={input} value={eps} placeholder={`${nets[0]?.name ?? "fast"}=192.0.2.10:9100`} onChange={(e) => setEps(e.target.value)} />
          </Field>
          <p className="text-xs text-slate-500">
            The node listens on them once restarted with them: run the installer on its host.
          </p>
        </ActionDialog>
      )}
      {port && (
        <ActionDialog
          title={`Role of port ${port.name} on node ${port.node}`}
          submit="Set"
          cli={`port role --node ${port.node} --name ${port.name} --role ${role}`}
          run={() =>
            cluster.setPortRole({ node: port.node, name: port.name, role: role === "default" ? undefined : role })
          }
          onClose={(done) => {
            setPort(null);
            if (done) ports.reload();
          }}
        >
          <Field label="Role">
            <select className={input} value={role} onChange={(e) => setRole(e.target.value)}>
              <option value="default">default (storage with endpoints, else host)</option>
              {ROLES.map((r) => (
                <option key={r}>{r}</option>
              ))}
            </select>
          </Field>
        </ActionDialog>
      )}
    </>
  );
}
