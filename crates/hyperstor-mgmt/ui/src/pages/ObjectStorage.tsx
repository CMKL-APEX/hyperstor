// Object storage (14-s3.md): every tenant's buckets as tenant/bucket
// (D246), read from the store by the map service (D254), with their objects
// and bytes (D256), and deleted only empty (D255); access keys, whose secrets are shown once (D231);
// placements, the pools new buckets go in (D232); and realms (D248).
import { useState } from "react";
import { Modal, TokenReveal } from "@glidex/ui-kit";
import { cluster, realms, s3 } from "../api";
import type { S3Bucket } from "../gen/hyperstor/v1/admin_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Table, fmtBytes, fmtTime, input, td, useCan, usePoll } from "../ui";

const named = (b: S3Bucket) => `${b.tenant}/${b.name}`;
/** A pool by name, or by id when it was deleted since. */
const pool = (name: string, id: number) => name || `#${id} (deleted)`;

function Buckets() {
  const can = useCan();
  const [tenant, setTenant] = useState("");
  const list = usePoll(async () => (await s3.listS3Buckets({ tenant: Number(tenant || "0") })).buckets, 10000);
  const [doomed, setDoomed] = useState<S3Bucket | null>(null);
  return (
    <Card
      title="Buckets"
      actions={
        <input
          className={`${input} w-40`}
          aria-label="Tenant"
          placeholder="every tenant"
          value={tenant}
          onChange={(e) => setTenant(e.target.value.replace(/\D/g, ""))}
          onBlur={() => list.reload()}
        />
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Bucket", "Realm", "Data pool", "Index pool", "Objects", "Size", "Created", ""]} empty="No buckets.">
        {(list.data ?? []).map((b) => (
          <tr key={named(b)} data-testid={`bucket-${b.tenant}-${b.name}`}>
            <td className={`${td} font-mono`}>{named(b)}</td>
            <td className={td}>{b.realm}</td>
            <td className={td}>{pool(b.dataPool, b.dataPoolId)}</td>
            <td className={td}>{pool(b.indexPool, b.indexPoolId)}</td>
            <td className={td}>{b.counted ? b.objects.toString() : "–"}</td>
            <td className={td}>{b.counted ? fmtBytes(b.bytes) : "–"}</td>
            <td className={td}>{fmtTime(b.createdUnix)}</td>
            <td className={td}>
              {can("store") && (
                <Button kind="danger" onClick={() => setDoomed(b)}>
                  Delete
                </Button>
              )}
            </td>
          </tr>
        ))}
      </Table>
      <p className="text-xs text-slate-500 mt-2">
        Buckets are made by tenants' keys (S3's CreateBucket). Objects and size count each object's latest version; “–”
        for a bucket made before buckets were counted.
      </p>
      <Cli cmd={`s3 bucket list${tenant ? ` --tenant ${tenant}` : ""}`} />
      {doomed && (
        <ActionDialog
          title={`Delete bucket ${named(doomed)}`}
          submit="Delete"
          kind="danger"
          cli={`s3 bucket delete --tenant ${doomed.tenant} --name ${doomed.name}`}
          run={() => s3.deleteS3Bucket({ tenant: doomed.tenant, name: doomed.name })}
          onClose={(done) => {
            setDoomed(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">Only an empty bucket is deleted: one with objects or uploads under way is refused.</p>
        </ActionDialog>
      )}
    </Card>
  );
}

function Keys() {
  const list = usePoll(async () => (await s3.listS3Keys({})).keys, 0);
  const [making, setMaking] = useState(false);
  const [tenant, setTenant] = useState("");
  const [description, setDescription] = useState("");
  const [made, setMade] = useState<{ id: string; secret: string } | null>(null);
  const [revoke, setRevoke] = useState("");
  return (
    <Card
      title="Access keys"
      actions={
        <Button kind="primary" onClick={() => setMaking(true)}>
          Make a key
        </Button>
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Key", "Tenant", "Realm", "Description", "Made", "By", ""]} empty="No access keys.">
        {(list.data ?? []).map((k) => (
          <tr key={k.id} data-testid={`s3key-${k.id}`}>
            <td className={`${td} font-mono text-xs`}>{k.id}</td>
            <td className={td}>{k.tenant}</td>
            <td className={td}>{k.realm}</td>
            <td className={td}>{k.description || "–"}</td>
            <td className={td}>{fmtTime(k.createdUnix)}</td>
            <td className={td}>{k.createdBy}</td>
            <td className={td}>
              <Button kind="danger" onClick={() => setRevoke(k.id)}>
                Revoke
              </Button>
            </td>
          </tr>
        ))}
      </Table>
      <Cli cmd="s3 key list" />
      {making && (
        <ActionDialog
          title="Make an access key"
          submit="Make"
          cli={`s3 key create --tenant ${tenant || "N"}${description ? ` --description '${description}'` : ""}`}
          run={async () => {
            const r = await s3.createS3Key({ tenant: Number(tenant || "0"), description });
            setMade({ id: r.key?.id ?? "", secret: r.secret });
          }}
          onClose={(done) => {
            setMaking(false);
            setTenant("");
            setDescription("");
            if (done) list.reload();
          }}
        >
          <Field label="Tenant">
            <input className={input} value={tenant} onChange={(e) => setTenant(e.target.value.replace(/\D/g, ""))} />
          </Field>
          <Field label="Description">
            <input className={input} value={description} onChange={(e) => setDescription(e.target.value)} />
          </Field>
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {made && (
        <Modal title={`Access key ${made.id}`} onClose={() => setMade(null)}>
          <p className="text-sm mb-2" data-testid="s3key-made">
            Key <span className="font-mono">{made.id}</span>, and its secret:
          </p>
          <TokenReveal
            secret={made.secret}
            warning="The secret is shown this once: copy it now. The S3 client takes both (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)."
            copiedHint="Copied."
          />
        </Modal>
      )}
      {revoke && (
        <ActionDialog
          title={`Revoke key ${revoke}`}
          submit="Revoke"
          kind="danger"
          cli={`s3 key revoke --id ${revoke}`}
          run={() => s3.revokeS3Key({ id: revoke })}
          onClose={(done) => {
            setRevoke("");
            if (done) list.reload();
          }}
        >
          <p className="text-sm">Gateways refuse it within seconds.</p>
        </ActionDialog>
      )}
    </Card>
  );
}

function Placements({ realmNames }: { realmNames: string[] }) {
  const can = useCan();
  const list = usePoll(async () => (await s3.listS3Placements({})).placements, 0);
  const status = usePoll(() => cluster.getStatus({}), 0);
  const pools = (status.data?.pools ?? []).filter((p) => p.id !== 0).map((p) => p.name);
  const [setting, setSetting] = useState(false);
  const [name, setName] = useState("");
  const [realm, setRealm] = useState("default");
  const [data, setData] = useState("");
  const [index, setIndex] = useState("");
  const [doomed, setDoomed] = useState<{ name: string; realm: string } | null>(null);
  const realmFlag = (r: string) => (r && r !== "default" ? ` --realm ${r}` : "");
  return (
    <Card
      title="Placements"
      actions={
        can("store") && (
          <Button
            kind="primary"
            onClick={() => {
              setData(pools[0] ?? "");
              setIndex(pools[0] ?? "");
              setSetting(true);
            }}
          >
            Set a placement
          </Button>
        )
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Realm", "Placement", "Data pool", "Index pool", ""]} empty="No placements: buckets can't be made until “default” names pools.">
        {(list.data ?? []).map((p) => (
          <tr key={`${p.realm}/${p.name}`} data-testid={`placement-${p.realm}-${p.name}`}>
            <td className={td}>{p.realm}</td>
            <td className={td}>{p.name}</td>
            <td className={td}>{pool(p.dataPool, p.dataPoolId)}</td>
            <td className={td}>{pool(p.indexPool, p.indexPoolId)}</td>
            <td className={td}>
              {can("store") && (
                <Button kind="danger" onClick={() => setDoomed({ name: p.name, realm: p.realm })}>
                  Delete
                </Button>
              )}
            </td>
          </tr>
        ))}
      </Table>
      <p className="text-xs text-slate-500 mt-2">
        CreateBucket uses “default” unless its LocationConstraint names another. Buckets keep the pools they were made in.
      </p>
      <Cli cmd="s3 placement list" />
      {setting && (
        <ActionDialog
          title="Set a placement"
          submit="Set"
          cli={`s3 placement set --name ${name || "N"}${realmFlag(realm)} --data-pool ${data} --index-pool ${index}`}
          run={() =>
            s3.setS3Placement({
              placement: { name, realm: realm === "default" ? "" : realm, dataPool: data, indexPool: index },
            })
          }
          onClose={(done) => {
            setSetting(false);
            setName("");
            if (done) list.reload();
          }}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Realm">
            <select className={input} value={realm} onChange={(e) => setRealm(e.target.value)}>
              {realmNames.map((r) => (
                <option key={r}>{r}</option>
              ))}
            </select>
          </Field>
          <Field label="Data pool">
            <select className={input} value={data} onChange={(e) => setData(e.target.value)}>
              {pools.map((p) => (
                <option key={p}>{p}</option>
              ))}
            </select>
          </Field>
          <Field label="Index pool (flash, ideally)">
            <select className={input} value={index} onChange={(e) => setIndex(e.target.value)}>
              {pools.map((p) => (
                <option key={p}>{p}</option>
              ))}
            </select>
          </Field>
        </ActionDialog>
      )}
      {doomed && (
        <ActionDialog
          title={`Delete placement ${doomed.name}`}
          submit="Delete"
          kind="danger"
          cli={`s3 placement delete --name ${doomed.name}${realmFlag(doomed.realm)}`}
          run={() => s3.deleteS3Placement({ name: doomed.name, realm: doomed.realm === "default" ? "" : doomed.realm })}
          onClose={(done) => {
            setDoomed(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">Buckets in its pools stay where they are.</p>
        </ActionDialog>
      )}
    </Card>
  );
}

export default function ObjectStorage() {
  const can = useCan();
  const list = usePoll(() => realms.listRealms({}), 0);
  const all = list.data?.realms ?? [];
  return (
    <>
      <Buckets />
      {can("secure") ? (
        <Keys />
      ) : (
        <Card title="Access keys">
          <p className="text-sm text-slate-600">Access keys are managed by security administrators.</p>
        </Card>
      )}
      <Placements realmNames={all.length ? all.map((r) => r.name) : ["default"]} />
      <Card title="Realms">
        <ErrorText error={list.error} />
        <Table head={["Realm", "Tenants", "Pools", "Description"]}>
          {all.map((r) => (
            <tr key={r.name} data-testid={`realm-${r.name}`}>
              <td className={td}>{r.name}</td>
              <td className={td}>{r.name === "default" ? "the others" : r.tenants.join(", ") || "–"}</td>
              <td className={td}>{r.pools.join(", ") || "–"}</td>
              <td className={td}>{r.description}</td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          Every realm may use: {list.data?.openPools.join(", ") || "no pool"}. Each realm has its own gateway
          (hyperstor-install --s3-realm).
        </p>
        <Cli cmd="realm list" />
      </Card>
    </>
  );
}
