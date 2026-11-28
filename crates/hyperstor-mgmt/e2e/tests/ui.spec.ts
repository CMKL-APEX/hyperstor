// The web UI in a browser against a real cluster (global-setup.ts): the
// first administrator's claim, roles, the pages and their actions,
// tokens shown once, single sign-on, logging out, and tenancy (realms,
// tenants, projects, and fake glidex directories' records and offers, one
// trusted as a whole). In order: one
// cluster, its people and volumes carry over.
import { expect, test, type Page } from "@playwright/test";

const url = () => process.env.HS_URL!;

async function logIn(page: Page, user: string, claim?: string) {
  await page.goto(url());
  if (claim) await page.getByLabel(/Claim code/).fill(claim);
  await page.getByLabel("Username").fill(user);
  await page.getByLabel("Password").fill(process.env.HS_PASSWORD!);
  await page.getByRole("button", { name: "Log in" }).click();
  await expect(page.getByTestId("whoami")).toContainText(user);
}

test.describe.configure({ mode: "serial" });

test("the first security administrator claims the cluster", async ({ page }) => {
  await page.goto(url());
  // A wrong password says so.
  await page.getByLabel("Username").fill("ann");
  await page.getByLabel("Password").fill("wrong");
  await page.getByRole("button", { name: "Log in" }).click();
  await expect(page.getByText("Wrong username or password.")).toBeVisible();
  await logIn(page, "ann", process.env.HS_CLAIM);
  await expect(page.getByTestId("whoami")).toContainText("security-admin");
  // The footer names the build, linked to its commit on GitHub.
  const commit = page.getByTestId("build-info").getByRole("link", { name: /^[0-9a-f]{10}$/ });
  await expect(commit).toHaveAttribute("href", /^https:\/\/github\.com\/.+\/commit\/[0-9a-f]{40}$/);
  // Security administrators manage people: ann takes storage too.
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-ann").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("checkbox", { name: "storage-admin" }).check();
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-ann")).toContainText("security-admin, storage-admin");
  // Your own new roles show at once, without logging in again.
  await expect(page.getByTestId("whoami")).toContainText("storage-admin");
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByRole("button", { name: "Create volume" })).toBeVisible();
});

test("the pages show the cluster", async ({ page }) => {
  await logIn(page, "ann");
  await expect(page.getByTestId("health")).toContainText("ok", { timeout: 20_000 });
  // A grid per pool, every PG clean (D154).
  await expect(page.getByTestId("pg-grid-1-clean")).toContainText(/[1-9]\d* clean/);
  await expect(page.getByTestId("pg-grid-1-inactive")).toContainText("0 inactive");
  // Shaded by usage once the primaries have reported (D155).
  await expect(page.getByTestId("pg-grid-1-usage")).toContainText("the fullest PG", { timeout: 20_000 });
  // The raw space of the devices in, summed (D156).
  await expect(page.getByTestId("cluster-space")).toContainText(/free of \d/, { timeout: 20_000 });
  await page.getByRole("link", { name: "Nodes" }).click();
  // Devices are numbered by node: 0, 1000, 2000.
  for (const d of [0, 1000, 2000]) await expect(page.getByTestId(`device-${d}`)).toContainText("in");
  // Each device's space once it has reported (D156).
  await expect(page.getByTestId("device-0-space")).toContainText(/free of \d/, { timeout: 20_000 });
  // Out and back in, with the command shown.
  const dev = page.getByTestId("device-2000");
  await dev.getByRole("button", { name: "Take out" }).click();
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl out --device 2000");
  await page.getByRole("dialog").getByRole("button", { name: "Take out" }).click();
  await expect(dev).toContainText("out");
  await dev.getByRole("button", { name: "Bring in" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Bring in" }).click();
  await expect(dev.getByText("in", { exact: true })).toBeVisible();
  // The map service member on each node's host (D282).
  await expect(page.getByTestId("node-0-member")).toContainText("member 1 (leader)");
  // And the host it runs on, as its node reports it (D348).
  await expect(page.getByTestId("node-0-host")).not.toHaveText("–", { timeout: 40_000 });
  await page.getByRole("link", { name: "Pools" }).click();
  await expect(page.getByTestId("pool-1-space")).toContainText(/free of \d/, { timeout: 20_000 });
  await expect(page.getByTestId("pool-1")).toBeVisible();
  await page.getByTestId("pool-1").getByRole("button", { name: "PGs" }).click();
  // The first cell is PG 1.0; picking it shows its sets, and only it.
  await page.getByRole("img", { name: /Pool 1's \d+ PGs/ }).click({ position: { x: 1, y: 1 } });
  await expect(page.getByText("1.0", { exact: true })).toBeVisible();
  await expect(page.getByText("1.1", { exact: true })).toHaveCount(0);
  await expect(page.getByText("hyperstorctl pgs --pool 1 --pg 0")).toBeVisible();
  await page.getByRole("button", { name: "List all" }).click();
  await expect(page.getByText("1.1", { exact: true })).toBeVisible();
  // The one member leads, in step in its own view (D281), on the host of
  // every node (D282).
  await page.getByRole("link", { name: "Members" }).click();
  const m1 = page.getByTestId("member-1");
  await expect(m1.getByText("leader")).toBeVisible();
  await expect(m1).toContainText("in step");
  await expect(m1).toContainText("nodes 0, 1, 2");
  await expect(page.getByTestId("raft-summary")).toContainText(/Term \d+, log at \d+/);
});

test("volumes and snapshots", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Volumes" }).click();
  await page.getByRole("button", { name: "Create volume" }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("web-vol");
  await dialog.getByLabel(/Size/).fill("64M");
  await expect(dialog.getByText("hyperstorctl create-volume --pool 1 --size 64M --name web-vol")).toBeVisible();
  await dialog.getByRole("button", { name: "Create" }).click();
  const row = page.getByTestId("volume-web-vol");
  await expect(row).toContainText("64.0 MiB");
  await row.getByRole("button", { name: "Snapshot", exact: true }).click();
  await page.getByRole("dialog").getByLabel("Name").fill("s1");
  await page.getByRole("dialog").getByRole("button", { name: "Take snapshot" }).click();
  await expect(page.getByTestId("snapshot-s1")).toBeVisible();
  await page.getByTestId("snapshot-s1").getByRole("button", { name: "Clone" }).click();
  await page.getByRole("dialog").getByLabel(/name/).fill("web-clone");
  await page.getByRole("dialog").getByRole("button", { name: "Clone" }).click();
  await expect(page.getByTestId("volume-web-clone")).toBeVisible();
  // A snapshot a clone depends on can't go: the API's reason shows.
  await page.getByTestId("snapshot-s1").getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Move to the trash" }).click();
  await expect(page.getByRole("dialog").getByRole("alert")).toContainText("in_use");
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  // Flattening is an operation: its id at once, its progress on Operations.
  await page.getByTestId("volume-web-clone").getByRole("button", { name: "Flatten" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Flatten" }).click();
  const notice = page.getByText(/Flattening: operation [0-9a-f]{16}/);
  await expect(notice).toBeVisible();
  const opId = /operation ([0-9a-f]{16})/.exec((await notice.textContent()) ?? "")![1];
  await page.getByRole("link", { name: "Operations" }).click();
  const flatten = page.getByTestId(`op-${opId}`);
  await expect(flatten).toContainText("flatten");
  await expect(flatten).toContainText("done", { timeout: 30_000 });
  await expect(flatten.getByRole("button", { name: "Cancel" })).toHaveCount(0);
  await page.getByRole("link", { name: "Volumes" }).click();
  // The clone goes, to the trash.
  await page.getByTestId("volume-web-clone").getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Move to the trash" }).click();
  await expect(page.getByTestId("volume-web-clone")).toHaveCount(0);
  await expect(page.getByText(/Moved to the trash: purged/)).toBeVisible();
});

test("pools, devices and volumes are managed", async ({ page }) => {
  await logIn(page, "ann");
  const dialog = page.getByRole("dialog");
  // A volume grows; the size starts at twice what it is.
  await page.getByRole("link", { name: "Volumes" }).click();
  const vol = page.getByTestId("volume-web-vol");
  await vol.getByRole("button", { name: "Expand" }).click();
  await expect(dialog.getByLabel(/New size/)).toHaveValue("128M");
  await expect(dialog.getByText(/hyperstorctl expand-volume --id [0-9a-f]+ --size 128M/)).toBeVisible();
  await dialog.getByRole("button", { name: "Expand" }).click();
  await expect(vol).toContainText("128.0 MiB");
  // A cache pool: made, changed, its devices chosen, deleted.
  await page.getByRole("link", { name: "Pools" }).click();
  await page.getByRole("button", { name: "Create pool" }).click();
  await dialog.getByLabel("Name").fill("web-pool");
  await dialog.getByLabel("Kind").selectOption("cache");
  await dialog.getByLabel(/^PGs/).fill("16");
  await dialog.getByLabel(/autoscale/).uncheck();
  await expect(
    dialog.getByText("hyperstorctl pool create --name web-pool --classes file --kind cache --pgs 16 --autoscale off"),
  ).toBeVisible();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page.getByText("Pool 3 web-pool made, 16 PGs.")).toBeVisible();
  const pool = page.getByTestId("pool-3");
  await expect(pool).toContainText("web-pool");
  await pool.getByRole("button", { name: "Edit" }).click();
  await dialog.getByLabel(/Seconds a PG waits/).fill("300");
  await expect(dialog.getByText("hyperstorctl pool set --pool 3 --cache-grace 300")).toBeVisible();
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("Pool 3 changed.")).toBeVisible();
  await pool.getByRole("button", { name: "Devices" }).click();
  const member = dialog.getByTestId("pool-device-2000");
  await expect(member).toContainText("yes");
  await member.getByRole("button", { name: "Stop using" }).click();
  await expect(member).toContainText("no (removed)");
  await member.getByRole("button", { name: "Use" }).click();
  await expect(member).toContainText("yes");
  await dialog.getByRole("button", { name: "Done" }).click();
  await pool.getByRole("button", { name: "Delete" }).click();
  await expect(dialog.getByText("hyperstorctl pool delete --pool 3")).toBeVisible();
  await dialog.getByRole("button", { name: "Delete" }).click();
  await expect(page.getByText("Pool 3 deleted.")).toBeVisible();
  await expect(pool).toHaveCount(0);
  // A device's weight set, then following its capacity again.
  await page.getByRole("link", { name: "Nodes" }).click();
  const dev = page.getByTestId("device-2000");
  await dev.getByRole("button", { name: "Weight" }).click();
  await dialog.getByLabel(/Weight/).fill("0.5");
  await expect(dialog.getByText("hyperstorctl weight --device 2000 --weight 0.5")).toBeVisible();
  await dialog.getByRole("button", { name: "Set weight" }).click();
  await expect(dev).toContainText("0.50");
  await dev.getByRole("button", { name: "Weight" }).click();
  await dialog.getByLabel(/Weight/).fill("auto");
  await dialog.getByRole("button", { name: "Set weight" }).click();
  await expect(dev).not.toContainText("0.50");
  // A device added to node 2 at weight 0, then removed: nothing moves.
  await page.getByRole("button", { name: "Add device" }).click();
  await dialog.getByRole("combobox", { name: /Node/ }).selectOption("2");
  await dialog.getByLabel(/Address/).fill("127.0.0.1:1");
  await expect(dialog.getByText("hyperstorctl device add --node 2 --address 127.0.0.1:1")).toBeVisible();
  await dialog.getByRole("button", { name: "Add" }).click();
  await expect(page.getByText(/Device 2001 added to node 2/)).toBeVisible();
  const added = page.getByTestId("device-2001");
  await expect(added).toContainText("0.00");
  await added.getByRole("button", { name: "Remove" }).click();
  await expect(dialog).toContainText("0 PG(s) move");
  await expect(dialog.getByText("hyperstorctl device remove --device 2001")).toBeVisible();
  await dialog.getByRole("button", { name: "Remove" }).click();
  await expect(page.getByText(/Removing device 2001: operation [0-9a-f]{16}/)).toBeVisible();
  await expect(added).toHaveCount(0, { timeout: 30_000 });
});

test("the load and the drives are shown", async ({ page }) => {
  await logIn(page, "ann");
  // Every device's last minute once it has reported (D218, D220).
  await page.getByRole("link", { name: "Nodes" }).click();
  for (const d of [0, 1000, 2000]) await expect(page.getByTestId(`perf-${d}`)).toBeVisible({ timeout: 20_000 });
  await page.getByTestId("perf-1000").click();
  const panel = page.getByTestId("device-panel-1000");
  await expect(panel).toContainText("a file: no drive to read");
  await expect(panel.getByRole("img", { name: "Requests per second (as primary)" })).toBeVisible();
  await expect(panel.getByText("hyperstorctl perf --device 1000")).toBeVisible();
  // The grid's load views (D219), with their command.
  await page.getByRole("link", { name: "Pools" }).click();
  await page.getByTestId("pool-1").getByRole("button", { name: "PGs" }).click();
  await page.getByTestId("grid-view-reads").click();
  await expect(page.getByTestId("pg-grid-1-heat")).toContainText("reads");
  await expect(page.getByText("hyperstorctl pg-grid --pool 1 --heat read")).toBeVisible();
  await page.getByTestId("grid-view-writes").click();
  await expect(page.getByTestId("pg-grid-1-heat")).toContainText("writes");
  // Busiest volumes, and moving one, with its command (D214).
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByText("Busiest volumes now")).toBeVisible();
  await page.getByRole("button", { name: "Move to pool" }).first().click();
  await expect(page.getByRole("dialog")).toContainText(/hyperstorctl move-volume --id [0-9a-f]+ --pool \d+ --wait 3600/);
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  // The Overview's load, and the Advisor (D222).
  await page.getByRole("link", { name: "Overview" }).click();
  await expect(page.getByTestId("load")).toContainText("requests / s");
  await page.getByRole("link", { name: "Advisor" }).first().click();
  await expect(page.getByTestId("findings")).toBeVisible();
  await expect(page.getByText("hyperstorctl advise")).toBeVisible();
});

test("networks are set, a device's endpoints too, and ports are shown", async ({ page }) => {
  await logIn(page, "ann");
  const dialog = page.getByRole("dialog");
  await page.getByRole("link", { name: "Networks" }).click();
  await expect(page.getByText("No networks: storage uses devices' addresses.")).toBeVisible();
  // A data network, as hyperstorctl network set makes it.
  await page.getByRole("button", { name: "Add network" }).click();
  await dialog.getByLabel(/^Name/).fill("fast");
  await dialog.getByLabel(/^Match/).fill("subnet 127.0.0.2/32");
  await expect(dialog.getByText('hyperstorctl network set --name fast --kind data --match "subnet 127.0.0.2/32"')).toBeVisible();
  await dialog.getByRole("button", { name: "Add" }).click();
  const net = page.getByTestId("network-fast");
  await expect(net).toContainText("subnet 127.0.0.2/32");
  // An endpoint on it for device 2000; the network then stays.
  const dev = page.getByTestId("endpoints-2000");
  await dev.getByRole("button", { name: "Endpoints" }).click();
  await dialog.getByLabel(/NETWORK=IP:PORT/).fill("fast=127.0.0.2:1");
  await expect(dialog.getByText("hyperstorctl device endpoints --device 2000 --endpoints fast=127.0.0.2:1")).toBeVisible();
  await dialog.getByRole("button", { name: "Set" }).click();
  await expect(dev).toContainText("fast 127.0.0.2:1");
  await expect(net.getByRole("button", { name: "Remove" })).toBeDisabled();
  // A bad match is refused, with the reason.
  await net.getByRole("button", { name: "Change" }).click();
  await dialog.getByLabel(/^Match/).fill("127.0.0.2");
  await dialog.getByRole("button", { name: "Change" }).click();
  await expect(dialog.getByRole("alert")).toContainText("subnet CIDR");
  await dialog.getByRole("button", { name: "Cancel" }).click();
  // Without the endpoint, the network goes.
  await dev.getByRole("button", { name: "Endpoints" }).click();
  await dialog.getByLabel(/NETWORK=IP:PORT/).fill("");
  await dialog.getByRole("button", { name: "Set" }).click();
  await expect(dev).toContainText("none");
  await net.getByRole("button", { name: "Remove" }).click();
  await expect(dialog.getByText("hyperstorctl network remove --name fast")).toBeVisible();
  await dialog.getByRole("button", { name: "Remove" }).click();
  await expect(net).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "Ports" })).toBeVisible();
});

test("a pool's scrub is shown, changed and started", async ({ page }) => {
  await logIn(page, "ann");
  const dialog = page.getByRole("dialog");
  // Its Scrub card: settings and coverage (28-scrub.md §31.8).
  await page.getByRole("link", { name: "Pools" }).click();
  await page.getByTestId("pool-1").getByRole("button", { name: "PGs" }).click();
  const card = page.getByTestId("scrub-1");
  await expect(card).toContainText("light-scrubbed every 24 h, and deep every 7 scrubs");
  await expect(page.getByTestId("scrub-1-deep")).toContainText("Oldest deep scrub");
  await expect(page.getByText("hyperstorctl scrub status --pool 1")).toBeVisible();
  // Weakening takes a reason (D226); restoring it doesn't.
  await page.getByRole("button", { name: "Change" }).click();
  await dialog.getByLabel(/Deep every/).fill("14");
  await expect(dialog.getByText("hyperstorctl scrub set --pool 1 --deep-every 14")).toBeVisible();
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(dialog).toContainText("Give a reason");
  await dialog.getByLabel(/Reason/).fill("browser test");
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(card).toContainText("reduced");
  await page.getByRole("button", { name: "Change" }).click();
  await dialog.getByLabel(/Deep every/).fill("7");
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(card).not.toContainText("reduced");
  // Scrub now, as an operation (D229).
  await page.getByRole("button", { name: "Scrub now" }).click();
  await expect(page.getByText(/Scrubbing \d+ PG\(s\): operation [0-9a-f]+/)).toBeVisible();
  // The grid's Scrub view, and a PG's last scrubs, as health links to it.
  await page.getByTestId("grid-view-scrub").click();
  await expect(page.getByTestId("pg-grid-1-scrub")).toContainText("last deep scrub");
  await expect(page.getByText("hyperstorctl scrub pgs --pool 1")).toBeVisible();
  await page.goto(`${url()}/pools?pool=1&view=scrub&pg=0`);
  await expect(page.getByTestId("pg-scrub")).toContainText("PG 1.0");
  await expect(page.getByTestId("pg-scrub")).toContainText("deep");
});

test("the trash restores and purges", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Trash" }).click();
  const row = page.getByTestId("trash-web-clone");
  await expect(row).toContainText("volume");
  await expect(page.getByTestId("trash-settings-1")).toContainText("kept 24 h");
  // Restored: back among the volumes.
  await row.getByRole("button", { name: "Restore" }).click();
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl trash restore --id");
  await page.getByRole("dialog").getByRole("button", { name: "Restore" }).click();
  await expect(row).toHaveCount(0);
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByTestId("volume-web-clone")).toBeVisible();
  // Deleted again, then purged at once.
  await page.getByTestId("volume-web-clone").getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Move to the trash" }).click();
  await page.getByRole("link", { name: "Trash" }).click();
  await row.getByRole("button", { name: "Purge now" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Purge now" }).click();
  await expect(page.getByText(/Purging volume web-clone: operation [0-9a-f]{16}/)).toBeVisible();
  // The purge runs in the background; the list catches up.
  await expect(page.getByText("The trash is empty.")).toBeVisible({ timeout: 30_000 });
});

test("the audit log shows who did what", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Audit" }).click();
  await page.getByLabel("Operation").fill("CreateVolume");
  // Records reach .meta within a second or two.
  await expect(async () => {
    await page.getByRole("button", { name: "Show" }).click();
    await expect(page.getByTestId("audit-record").first()).toContainText("CreateVolume", { timeout: 1000 });
  }).toPass({ timeout: 20_000 });
  await expect(page.getByTestId("audit-record").first()).toContainText("user:ann");
  await expect(page.getByText("hyperstorctl audit list --op CreateVolume")).toBeVisible();
  await expect(page.getByTestId("audit-retention")).toContainText("kept 365 days");
  const download = page.waitForEvent("download");
  await page.getByRole("button", { name: "Export" }).click();
  expect((await download).suggestedFilename()).toBe("hyperstor-audit.jsonl");
});

test("an API token is shown once", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByRole("tab", { name: "Your API tokens" }).click();
  await page.getByRole("button", { name: "Make a token" }).click();
  await page.getByRole("dialog").getByLabel(/Name/).fill("ci");
  await page.getByRole("dialog").getByRole("button", { name: "Make" }).click();
  const secret = page.getByRole("dialog").locator("input[readonly]");
  await expect(secret).toHaveAttribute("type", "password");
  await expect(secret).toHaveValue(/^hst_/);
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await expect(page.getByRole("cell", { name: "ci", exact: true })).toBeVisible();
});

test("object storage: keys shown once, placements, buckets and realms", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Object storage" }).click();
  // Buckets are read from the store by the map service; none made yet.
  await expect(page.getByText("No buckets.")).toBeVisible();
  await expect(page.getByText("hyperstorctl s3 bucket list")).toBeVisible();
  // An access key: its secret shown once, then only its id.
  await page.getByRole("button", { name: "Make a key" }).click();
  await page.getByRole("dialog").getByLabel("Tenant").fill("3");
  await page.getByRole("dialog").getByLabel("Description").fill("ui key");
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl s3 key create --tenant 3 --description 'ui key'");
  await page.getByRole("dialog").getByRole("button", { name: "Make" }).click();
  const secret = page.getByRole("dialog").locator("input[readonly]");
  await expect(secret).toHaveAttribute("type", "password");
  await expect(secret).toHaveValue(/^.{40}$/);
  const id = ((await page.getByTestId("s3key-made").textContent()) ?? "").match(/HS[0-9A-Z]{18}/)![0];
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  const key = page.getByTestId(`s3key-${id}`);
  await expect(key).toContainText("ui key");
  await key.getByRole("button", { name: "Revoke" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Revoke" }).click();
  await expect(key).toHaveCount(0);
  // A placement set and deleted.
  await page.getByRole("button", { name: "Set a placement" }).click();
  await page.getByRole("dialog").getByLabel("Name").fill("ui");
  await page.getByRole("dialog").getByLabel("Data pool").selectOption("vol");
  await page.getByRole("dialog").getByLabel(/Index pool/).selectOption("vol");
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl s3 placement set --name ui --data-pool vol --index-pool vol");
  await page.getByRole("dialog").getByRole("button", { name: "Set" }).click();
  const placement = page.getByTestId("placement-default-ui");
  await expect(placement).toContainText("vol");
  await placement.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
  await expect(placement).toHaveCount(0);
  // Realms: "default" holds every tenant not put in another.
  await expect(page.getByTestId("realm-default")).toContainText("the others");
});

test("someone with roles in a realm acts only there", async ({ page }) => {
  // cy appears once logged in, with no roles.
  await logIn(page, "cy");
  await expect(page.getByText("You have no roles yet")).toBeVisible();
  await page.getByRole("button", { name: "Log out" }).click();
  // ann makes a volume of acme's tenant 7 and one of no tenant, and gives
  // cy storage-admin in acme.
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Volumes" }).click();
  for (const [name, tenant] of [
    ["acme-ui", "7"],
    ["plain-ui", ""],
  ]) {
    await page.getByRole("button", { name: "Create volume" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Name").fill(name);
    await dialog.getByLabel(/Size/).fill("4M");
    await dialog.getByLabel(/Tenant/).fill(tenant);
    await dialog.getByRole("button", { name: "Create" }).click();
    await expect(page.getByTestId(`volume-${name}`)).toBeVisible();
  }
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-cy").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("dialog").getByLabel("Roles in realms").fill("storage-admin@acme");
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-cy")).toContainText("storage-admin@acme");
  await page.getByRole("button", { name: "Log out" }).click();
  // cy acts in acme: its pages alone, its tenants' volumes alone.
  await logIn(page, "cy");
  await expect(page.getByLabel("Realm")).toHaveValue("acme");
  await expect(page.getByRole("option", { name: "Whole cluster" })).toHaveCount(0);
  for (const hidden of ["Overview", "Nodes", "Pools", "Networks", "Members", "Audit"]) {
    await expect(page.getByRole("link", { name: hidden })).toHaveCount(0);
  }
  // Its operations (D283).
  await page.getByRole("link", { name: "Operations" }).click();
  await expect(page.getByText("hyperstorctl --in-realm acme op list")).toBeVisible();
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByTestId("volume-acme-ui")).toBeVisible();
  await expect(page.getByTestId("volume-plain-ui")).toHaveCount(0);
  await expect(page.getByText("Busiest volumes now")).toHaveCount(0);
  // Its volumes go only to its tenants.
  await page.getByRole("button", { name: "Create volume" }).click();
  let dialog = page.getByRole("dialog");
  await dialog.getByLabel("Name").fill("cy-vol");
  await dialog.getByLabel(/Size/).fill("4M");
  await dialog.getByLabel(/Tenant/).fill("8");
  await expect(dialog.getByText("hyperstorctl --in-realm acme create-volume")).toBeVisible();
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(dialog.getByRole("alert")).toContainText("outside_realm");
  await dialog.getByLabel(/Tenant/).fill("7");
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page.getByTestId("volume-cy-vol")).toBeVisible();
  await page.getByRole("button", { name: "Log out" }).click();
  // ann may act in acme too, or in the whole cluster again.
  await logIn(page, "ann");
  await page.getByLabel("Realm").selectOption("acme");
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByTestId("volume-cy-vol")).toBeVisible();
  await expect(page.getByTestId("volume-plain-ui")).toHaveCount(0);
  await page.getByLabel("Realm").selectOption("");
  await expect(page.getByTestId("volume-plain-ui")).toBeVisible();
  // The whole cluster's lists name each row's realm (D286).
  await expect(page.getByTestId("volume-acme-ui")).toContainText("acme (tenant 7)");
  await expect(page.getByTestId("volume-plain-ui")).toContainText("default");
  await expect(page.getByRole("link", { name: "Nodes" })).toBeVisible();
});

test("someone without roles sees nothing to do", async ({ page }) => {
  await logIn(page, "bob");
  await expect(page.getByText("You have no roles yet")).toBeVisible();
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByRole("button", { name: "Create volume" })).toHaveCount(0);
  await page.getByRole("link", { name: "Pools" }).click();
  await expect(page.getByRole("button", { name: "Create pool" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Edit" })).toHaveCount(0);
  await page.getByRole("link", { name: "Nodes" }).click();
  await expect(page.getByRole("button", { name: "Add device" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Remove" })).toHaveCount(0);
  await page.getByRole("link", { name: "Security" }).click();
  await expect(page.getByRole("tab", { name: "People" })).toHaveCount(0);
  await page.getByRole("button", { name: "Log out" }).click();
  await expect(page.getByLabel("Username")).toBeVisible();
});

test("single sign-on with the identity provider", async ({ page }) => {
  await page.goto(`${url()}/volumes`);
  await page.getByRole("button", { name: "Sign in with your identity provider" }).click();
  await expect(page.getByTestId("whoami")).toContainText(process.env.HS_SSO!);
  // Back where the login began.
  await expect(page).toHaveURL(/\/volumes$/);
});

test("the tenant and project pickers show theirs, and new volumes go there", async ({ page }) => {
  // ui-e2e-cluster made tenant lab, its project web, and a volume in each
  // of lab's projects.
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByTestId("volume-lab-default-vol")).toBeVisible();
  await expect(page.getByTestId("volume-plain-ui")).toBeVisible();
  await page.getByLabel("Tenant", { exact: true }).selectOption({ label: "tenant lab" });
  await expect(page.getByTestId("volume-lab-default-vol")).toBeVisible();
  await expect(page.getByTestId("volume-lab-web-vol")).toBeVisible();
  await expect(page.getByTestId("volume-plain-ui")).toHaveCount(0);
  await page.getByLabel("Project", { exact: true }).selectOption({ label: "project web" });
  await expect(page.getByTestId("volume-lab-web-vol")).toBeVisible();
  await expect(page.getByTestId("volume-lab-default-vol")).toHaveCount(0);
  // A new volume goes to the project picked.
  await page.getByRole("button", { name: "Create volume" }).click();
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByText(/as picked above/)).toBeVisible();
  await expect(dialog.getByText(/create-volume .* --project \d+/)).toBeVisible();
  await dialog.getByLabel("Name").fill("picked-vol");
  await dialog.getByLabel(/Size/).fill("4M");
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page.getByTestId("volume-picked-vol")).toBeVisible();
  // The choice is kept on this browser, and "All tenants" shows all again.
  await page.reload();
  await expect(page.getByLabel("Project", { exact: true })).toHaveValue(/\d+/);
  await page.getByLabel("Tenant", { exact: true }).selectOption("");
  await expect(page.getByTestId("volume-plain-ui")).toBeVisible();
});

test("someone with roles in a tenant and a project only sees theirs", async ({ page }) => {
  // As a glidex directory's people have them (D321): no realm to act in.
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-bob").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("dialog").getByLabel("Roles in realms").fill("storage-admin@tenant:lab, viewer@project:lab/web");
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-bob")).toContainText("storage-admin@tenant:lab");
  await page.getByRole("button", { name: "Log out" }).click();
  await logIn(page, "bob");
  // No realm picked for them, and every call answers (it was refused
  // with "a realm is 1-63 of a-z, 0-9 and -").
  await expect(page.getByLabel("Realm")).toHaveCount(0);
  for (const hidden of ["Overview", "Nodes", "Pools", "Members", "Audit"]) {
    await expect(page.getByRole("link", { name: hidden })).toHaveCount(0);
  }
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByTestId("volume-lab-default-vol")).toBeVisible();
  await expect(page.getByTestId("volume-lab-web-vol")).toBeVisible();
  await expect(page.getByTestId("volume-plain-ui")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Create volume" })).toBeVisible();
  await expect(page.getByText("invalid_argument")).toHaveCount(0);
  await page.getByRole("link", { name: "Security" }).click();
  await expect(page.getByRole("tab", { name: "Join tokens" })).toHaveCount(0);
  await page.getByRole("button", { name: "Log out" }).click();
  // Back to none, for the tests after.
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-bob").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("dialog").getByLabel("Roles in realms").fill("");
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-bob")).toContainText("none");
});

test("a browser holds two accounts and switches between them", async ({ page }) => {
  // A password's account and the identity provider's: one each.
  await logIn(page, "ann");
  await page.getByRole("button", { name: "Accounts" }).click();
  await page.getByRole("menuitem", { name: "Add another account" }).click();
  // The login page, with ann's account to continue as.
  await expect(page.getByTestId("held-accounts")).toContainText("Continue as ann");
  await page.getByRole("button", { name: "Sign in with your identity provider" }).click();
  await expect(page.getByTestId("whoami")).toContainText(process.env.HS_SSO!);
  await page.getByRole("button", { name: "Accounts" }).click();
  await expect(page.getByTestId("account-ann")).toBeVisible();
  await page.getByTestId("account-ann").getByRole("menuitem", { name: "Switch" }).click();
  await expect(page.getByTestId("whoami")).toContainText("ann");
  // The provider's account is held now; removing it ends it.
  await page.getByRole("button", { name: "Accounts" }).click();
  const sso = page.getByTestId(`account-${process.env.HS_SSO!}`);
  await expect(sso).toBeVisible();
  await sso.getByRole("menuitem", { name: "Remove" }).click();
  await expect(sso).toHaveCount(0);
  await page.getByRole("menuitem", { name: "Sign out of all accounts" }).click();
  await expect(page.getByLabel("Username")).toBeVisible();
  await expect(page.getByTestId("held-accounts")).toHaveCount(0);
});

test("someone whose name has an @ is given roles and deleted", async ({ page }) => {
  // The identity provider's person is named by their email; a glidex
  // directory's by <id>@<realm>. The UI puts the name in the path,
  // encoded, which mgmt read undecoded ("not a user name").
  const sso = process.env.HS_SSO!;
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  const row = page.getByTestId(`person-${sso}`);
  await row.getByRole("button", { name: "Roles" }).click();
  await page.getByRole("dialog").getByRole("checkbox", { name: "viewer" }).check();
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(row).toContainText("viewer");
  await row.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(row).toHaveCount(0);
});

/** The tenancy page's tab `name`. */
async function tenancy(page: Page, tab: string) {
  await page.getByRole("link", { name: "Tenancy" }).click();
  await page.getByRole("tab", { name: tab }).click();
}

/** A delete confirmed by typing `name`: refused until it is typed. */
async function typedDelete(page: Page, name: string) {
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "Delete" })).toBeDisabled();
  await dialog.getByLabel("Confirm name").fill(name);
  await dialog.getByRole("button", { name: "Delete" }).click();
  await expect(dialog).toHaveCount(0);
}

test("tenancy: realms are made, bound and deleted", async ({ page }) => {
  await logIn(page, "ann");
  await tenancy(page, "Realms");
  await expect(page.getByTestId("tenancy-realm-default")).toContainText("can't be deleted");
  await expect(page.getByTestId("tenancy-realm-default").getByRole("button", { name: "Delete" })).toHaveCount(0);
  await page.getByRole("button", { name: "Create realm" }).click();
  let dialog = page.getByRole("dialog");
  await dialog.getByLabel(/^Name/).fill("ui-realm");
  await dialog.getByLabel(/Description/).fill("made in the browser");
  await expect(dialog).toContainText("hyperstorctl realm create --name ui-realm --description 'made in the browser'");
  await dialog.getByRole("button", { name: "Create" }).click();
  const realm = page.getByTestId("tenancy-realm-ui-realm");
  await expect(realm).toContainText("made in the browser");
  // A pool listed for it, then for every realm again.
  await page.getByTestId("tenancy-pool-1").getByRole("button", { name: "Change" }).click();
  dialog = page.getByRole("dialog");
  await dialog.getByRole("checkbox", { name: "realm ui-realm" }).check();
  await expect(dialog).toContainText("hyperstorctl realm pool --pool 1 --realm ui-realm");
  await dialog.getByRole("button", { name: "Save" }).click();
  // Its volumes of other realms keep it from being ui-realm's alone: the
  // server's reason shows.
  await expect(dialog.getByRole("alert")).toContainText("pool_in_use");
  await dialog.getByRole("checkbox", { name: "realm default" }).check();
  await dialog.getByRole("checkbox", { name: "realm acme" }).check();
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByTestId("tenancy-pool-1")).toContainText("ui-realm");
  await expect(realm).toContainText("vol");
  // The realm can't go while a pool names it: the server's reason shows.
  await realm.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByLabel("Confirm name").fill("ui-realm");
  await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("dialog").getByRole("alert")).toBeVisible();
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  await page.getByTestId("tenancy-pool-1").getByRole("button", { name: "Change" }).click();
  dialog = page.getByRole("dialog");
  for (const r of ["ui-realm", "default", "acme"]) await dialog.getByRole("checkbox", { name: `realm ${r}` }).uncheck();
  await expect(dialog).toContainText("hyperstorctl realm pool --pool 1 --realm any");
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("tenancy-pool-1")).toContainText("every realm");
  // A host and a management client bound, then unbound (any, D327).
  for (const [what, name] of [
    ["host", "ui-host"],
    ["client", "ui-client"],
  ]) {
    await page.getByRole("button", { name: what === "host" ? "Bind a host" : "Bind a client" }).click();
    dialog = page.getByRole("dialog");
    await dialog.getByLabel(what === "host" ? /^Host/ : /^Client/).fill(name);
    await dialog.getByRole("checkbox", { name: "realm ui-realm" }).check();
    await expect(dialog).toContainText(`hyperstorctl realm ${what} --${what} ${name} --realm ui-realm`);
    await dialog.getByRole("button", { name: "Save" }).click();
    const row = page.getByTestId(`tenancy-${what}-${name}`);
    await expect(row).toContainText("ui-realm");
    await expect(realm).toContainText(name);
    if (what === "client") {
      // Let make VM hosts' join tokens, then no longer (D382).
      const tokens = page.getByTestId(`tenancy-client-tokens-${name}`);
      await expect(tokens).toHaveText("–");
      await row.getByRole("button", { name: "Allow host tokens" }).click();
      await expect(page.getByRole("dialog")).toContainText(`hyperstorctl realm client-tokens --client ${name} --role host`);
      await page.getByRole("dialog").getByRole("button", { name: "Allow", exact: true }).click();
      await expect(tokens).toHaveText("host");
      await row.getByRole("button", { name: "No host tokens" }).click();
      await expect(page.getByRole("dialog")).toContainText(`--role none`);
      await page.getByRole("dialog").getByRole("button", { name: "Take away" }).click();
      await expect(tokens).toHaveText("–");
    }
    await row.getByRole("button", { name: "Unbind" }).click();
    await expect(page.getByRole("dialog")).toContainText(`--realm any`);
    await page.getByRole("dialog").getByRole("button", { name: "Unbind" }).click();
    await expect(row).toHaveCount(0);
  }
  // Deleted once nothing names it, after its name is typed.
  await realm.getByRole("button", { name: "Delete" }).click();
  await typedDelete(page, "ui-realm");
  await expect(realm).toHaveCount(0);
});

test("tenancy: tenants and projects are made, renamed, moved and deleted", async ({ page }) => {
  await logIn(page, "ann");
  await tenancy(page, "Tenants & projects");
  // lab's volumes count to it and its projects (D335).
  await expect(page.getByTestId("space-tenant-lab")).toContainText("head", { timeout: 30_000 });
  await expect(page.getByTestId("space-project-lab-web")).toContainText("head");
  // acme's tenant 7, used before records, may be named.
  await expect(page.getByTestId("tree-bare-7")).toBeVisible();
  await page.getByRole("button", { name: "Create tenant" }).click();
  let dialog = page.getByRole("dialog");
  await dialog.getByLabel(/^Name/).fill("ui-t");
  await expect(dialog).toContainText("hyperstorctl tenant create --name ui-t");
  await dialog.getByRole("button", { name: "Create" }).click();
  await expect(page.getByTestId("tree-project-ui-t-default")).toBeVisible();
  await page.getByTestId("tree-tenant-ui-t").getByRole("button", { name: "Rename" }).click();
  await page.getByRole("dialog").getByLabel("New name").fill("ui-t2");
  await page.getByRole("dialog").getByRole("button", { name: "Rename" }).click();
  const tenant = page.getByTestId("tree-tenant-ui-t2");
  await expect(tenant).toBeVisible();
  // A default project keeps its name and tenant.
  await expect(page.getByTestId("tree-project-ui-t2-default").getByRole("button")).toHaveCount(0);
  await tenant.getByRole("button", { name: "New project" }).click();
  await page.getByRole("dialog").getByLabel(/^Name/).fill("ui-p");
  await page.getByRole("dialog").getByRole("button", { name: "Create" }).click();
  await page.getByTestId("tree-project-ui-t2-ui-p").getByRole("button", { name: "Rename" }).click();
  await page.getByRole("dialog").getByLabel("New name").fill("ui-p2");
  await page.getByRole("dialog").getByRole("button", { name: "Rename" }).click();
  // Moved to lab, as an operation followed here (D303).
  await page.getByTestId("tree-project-ui-t2-ui-p2").getByRole("button", { name: "Move" }).click();
  dialog = page.getByRole("dialog");
  await dialog.getByLabel("To tenant").selectOption({ label: "lab (realm default)" });
  await expect(dialog).toContainText(/hyperstorctl project move --project \d+ --to \d+/);
  await dialog.getByRole("button", { name: "Move" }).click();
  await expect(page.getByTestId(/^tenancy-op-/)).toContainText("done", { timeout: 30_000 });
  await expect(page.getByTestId("tree-project-lab-ui-p2")).toBeVisible();
  await expect(page.getByTestId("tree-project-ui-t2-ui-p2")).toHaveCount(0);
  await page.getByTestId("tree-project-lab-ui-p2").getByRole("button", { name: "Delete" }).click();
  await typedDelete(page, "ui-p2");
  await expect(page.getByTestId("tree-project-lab-ui-p2")).toHaveCount(0);
  // In another realm, a tenant can't be deleted: the reason shows.
  await tenant.getByRole("button", { name: "Realm", exact: true }).click();
  dialog = page.getByRole("dialog");
  await dialog.getByRole("combobox").selectOption("acme");
  await expect(dialog).toContainText(/hyperstorctl realm tenant --tenant \d+ --realm acme/);
  await dialog.getByRole("button", { name: "Move" }).click();
  await expect(dialog).toHaveCount(0);
  await tenant.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByLabel("Confirm name").fill("ui-t2");
  await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("dialog").getByRole("alert")).toBeVisible();
  await page.getByRole("dialog").getByRole("button", { name: "Cancel" }).click();
  await tenant.getByRole("button", { name: "Realm", exact: true }).click();
  await page.getByRole("dialog").getByRole("combobox").selectOption("default");
  await page.getByRole("dialog").getByRole("button", { name: "Move" }).click();
  await tenant.getByRole("button", { name: "Delete" }).click();
  await typedDelete(page, "ui-t2");
  await expect(tenant).toHaveCount(0);
});

test("tenancy: a directory's records are read-only, offers are shown once and revoked", async ({ page }) => {
  // ui-e2e-cluster's fake directory uni: its tenant physics and project
  // optics, made by its feed.
  await logIn(page, "ann");
  await tenancy(page, "Tenants & projects");
  const physics = page.getByTestId("tree-tenant-physics");
  await expect(physics).toContainText("from directory uni");
  await expect(physics.getByRole("link", { name: "open in glidex" })).toHaveAttribute("href", /\/tenants\/t1$/);
  for (const b of ["Rename", "Realm", "Delete"]) await expect(physics.getByRole("button", { name: b, exact: true })).toHaveCount(0);
  const optics = page.getByTestId("tree-project-physics-optics");
  await expect(optics.getByRole("link", { name: "open in glidex" })).toHaveAttribute("href", /\/projects\/p1$/);
  await expect(optics.getByRole("button")).toHaveCount(0);
  // The directory: its realm, feed and staleness limit (D324).
  await page.getByRole("tab", { name: "Directories" }).click();
  const uni = page.getByTestId("directory-uni");
  await expect(uni).toContainText("realm uni");
  await expect(uni).toContainText("feed fresh");
  await expect(page.getByTestId("staleness-uni")).toContainText("60 minutes");
  await page.getByTestId("staleness-uni").getByRole("button", { name: "Change" }).click();
  await page.getByRole("dialog").getByLabel(/^Minutes/).fill("30");
  await expect(page.getByRole("dialog")).toContainText("--minutes 30");
  await page.getByRole("dialog").getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("staleness-uni")).toContainText("30 minutes");
  // The operator tenant, named then cleared.
  await page.getByTestId("trusted-physics").getByRole("button", { name: "Make operator" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Make operator" }).click();
  await expect(page.getByTestId("operator-tenant")).toContainText("physics");
  await page.getByTestId("operator-tenant").getByRole("button", { name: "Clear" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Clear" }).click();
  await expect(page.getByTestId("operator-tenant")).toContainText("none");
  // An offer: its token shown once, never kept by the page.
  const issuer = ((await uni.locator("span.font-medium").first().textContent()) ?? "").trim();
  await page.getByRole("button", { name: "Make an offer" }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel(/^glidex tenant/).fill("chem");
  await dialog.getByLabel(/^Directory/).fill(issuer);
  // The pin is mgmt's own certificate's, from mgmt (D343).
  await expect(dialog.getByLabel("Pin")).toHaveValue(/^[0-9a-f]{64}$/);
  const pin = await dialog.getByLabel("Pin").inputValue();
  await expect(dialog).toContainText(`hyperstorctl admission offer --tenant chem --directory ${issuer} --url https://`);
  await dialog.getByRole("button", { name: "Make" }).click();
  await expect(page.getByTestId("offer-made")).toContainText(pin);
  const secret = page.getByRole("dialog").getByLabel("offer token");
  await expect(secret).toHaveAttribute("type", "password");
  const token = await secret.inputValue();
  expect(token).toMatch(/^gxo_/);
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  const offer = page.getByTestId("offer-chem");
  await expect(offer).toContainText("open");
  expect(await page.content()).not.toContain(token);
  expect(page.url()).not.toContain(token);
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }) + JSON.stringify({ ...sessionStorage }));
  expect(stored).not.toContain(token);
  await offer.getByRole("button", { name: "Revoke" }).click();
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl admission revoke --id");
  await page.getByRole("dialog").getByRole("button", { name: "Revoke" }).click();
  await expect(offer).toContainText("revoked");
  await expect(offer.getByRole("button", { name: "Revoke" })).toHaveCount(0);
  // Another pin, pasted, is the offer's instead.
  await page.getByRole("button", { name: "Make an offer" }).click();
  await dialog.getByLabel(/^glidex tenant/).fill("bio");
  await dialog.getByLabel(/^Directory/).fill(issuer);
  await dialog.getByRole("button", { name: "Use another" }).click();
  await dialog.getByLabel(/^SHA-256/).fill("AB:".repeat(31) + "AB");
  await expect(dialog).toContainText(`--pin ${"AB:".repeat(31)}AB`);
  await dialog.getByRole("button", { name: "Make" }).click();
  await expect(page.getByTestId("offer-made")).toContainText("ab".repeat(32));
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await page.getByTestId("offer-bio").getByRole("button", { name: "Revoke" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Revoke" }).click();
  await expect(page.getByTestId("offer-bio")).toContainText("revoked");
});

test("tenancy: a directory trusted as a whole: opt-ins decided, its setting changed, offered and ended", async ({ page }) => {
  // ui-e2e-cluster's school: trusted as a whole (D345), opt-ins held for
  // approval (D346), each capped at 1 GiB (D347); art and music wait.
  await logIn(page, "ann");
  await tenancy(page, "Directories");
  const whole = page.getByTestId("whole-school");
  const dialog = page.getByRole("dialog");
  await expect(whole).toContainText("trusted as a whole");
  await expect(whole).toContainText("approval");
  await expect(whole).toContainText("1.0 GiB");
  await expect(page.getByTestId("admission-art")).toContainText("pending");
  // Approved: its tenant made here with a copy of the default caps.
  await page.getByTestId("admission-art").getByRole("button", { name: "Approve" }).click();
  await expect(dialog).toContainText(/hyperstorctl admission approve --directory \S+ --tenant art/);
  await dialog.getByRole("button", { name: "Approve" }).click();
  await expect(page.getByTestId("admission-art")).toHaveCount(0);
  await expect(page.getByTestId("trusted-art")).toContainText("caps");
  await expect(page.getByTestId("trusted-art")).toContainText("1.0 GiB");
  // Refused: listed, with nothing more to decide.
  await page.getByTestId("admission-music").getByRole("button", { name: "Refuse" }).click();
  await expect(dialog).toContainText(/hyperstorctl admission reject --directory \S+ --tenant music/);
  await dialog.getByRole("button", { name: "Refuse" }).click();
  await expect(page.getByTestId("admission-music")).toContainText("refused");
  await expect(page.getByTestId("admission-music").getByRole("button")).toHaveCount(0);
  // The setting and default caps changed; art keeps its own (D347).
  await whole.getByRole("button", { name: "Change admission" }).click();
  const caps = dialog.getByLabel(/^Default caps/);
  const pool = (await caps.inputValue()).split("=")[0];
  expect(pool).not.toBe("");
  await dialog.getByLabel("Opt-ins").selectOption("open");
  await caps.fill("nonsense");
  await expect(dialog.getByRole("button", { name: "Save" })).toBeDisabled();
  await caps.fill(`${pool}=2G`);
  await expect(dialog).toContainText(`--admit open --cap ${pool}=${2n << 30n}`);
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(whole).toContainText("each is admitted at once");
  await expect(whole).toContainText("2.0 GiB");
  await expect(page.getByTestId("trusted-art")).toContainText("1.0 GiB");
  // An offer to another whole directory, its token shown once.
  await page.getByRole("button", { name: "Make an offer" }).click();
  await dialog.getByLabel("The whole directory").check();
  await expect(dialog.getByLabel(/^glidex tenant/)).toHaveCount(0);
  await dialog.getByLabel(/^Directory/).fill("https://id.example/d/campus");
  await expect(page.getByTestId("whole-offer-note")).toContainText("choose approval");
  await dialog.getByLabel("Opt-ins").selectOption("approval");
  await expect(page.getByTestId("whole-offer-note")).not.toContainText("choose approval");
  await dialog.getByLabel(/^Default caps/).fill(`${pool}=5G`);
  await expect(dialog).toContainText("hyperstorctl admission offer --directory https://id.example/d/campus --url https://");
  await expect(dialog).toContainText(`--admit approval --cap ${pool}=${5n << 30n}`);
  await dialog.getByRole("button", { name: "Make" }).click();
  await expect(page.getByTestId("offer-made")).toContainText("the whole directory");
  await expect(dialog).toContainText("gxctl directory trust-cluster --offer TOKEN");
  const token = await dialog.getByLabel("offer token").inputValue();
  expect(token).toMatch(/^gxo_/);
  await dialog.getByRole("button", { name: "Close" }).click();
  const offer = page.getByTestId("offer-directory-campus");
  await expect(offer).toContainText("the whole directory (approval)");
  await expect(offer).toContainText("open");
  expect(await page.content()).not.toContain(token);
  await offer.getByRole("button", { name: "Revoke" }).click();
  await dialog.getByRole("button", { name: "Revoke" }).click();
  await expect(offer).toContainText("revoked");
  // Ended as a whole: no longer trusted; art stays, unlinked (D316).
  await whole.getByRole("button", { name: "End trust in the directory" }).click();
  await expect(dialog).toContainText(/hyperstorctl admission end --directory \S+/);
  await expect(dialog).not.toContainText("--tenant");
  await dialog.getByRole("button", { name: "End trust" }).click();
  await expect(page.getByTestId("directory-school")).toHaveCount(0);
  await page.getByRole("tab", { name: "Tenants & projects" }).click();
  const art = page.getByTestId("tree-tenant-art");
  await expect(art).toBeVisible();
  await expect(art).not.toContainText("from directory");
});

test("tenancy: a viewer sees realms and tenants with no actions", async ({ page }) => {
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-bob").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("checkbox", { name: "viewer" }).check();
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-bob")).toContainText("viewer");
  await page.getByRole("button", { name: "Log out" }).click();
  await logIn(page, "bob");
  await tenancy(page, "Realms");
  await expect(page.getByTestId("tenancy-realm-acme")).toBeVisible();
  await expect(page.getByRole("tab", { name: "Directories" })).toHaveCount(0);
  for (const b of ["Create realm", "Delete", "Change", "Unbind", "Bind a host", "Bind a client", "Allow host tokens"])
    await expect(page.getByRole("button", { name: b, exact: true })).toHaveCount(0);
  await page.getByRole("tab", { name: "Tenants & projects" }).click();
  await expect(page.getByTestId("tree-tenant-lab")).toBeVisible();
  for (const b of ["Create tenant", "New project", "Rename", "Realm", "Move", "Delete", "Name it"])
    await expect(page.getByRole("button", { name: b, exact: true })).toHaveCount(0);
});

test("tenancy: a security administrator alone reads realms and tenants", async ({ page }) => {
  // bob: security-admin, and no read role (I413).
  await logIn(page, "ann");
  await page.getByRole("link", { name: "Security" }).click();
  await page.getByTestId("person-bob").getByRole("button", { name: "Roles" }).click();
  await page.getByRole("checkbox", { name: "viewer" }).uncheck();
  await page.getByRole("checkbox", { name: "security-admin" }).check();
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByTestId("person-bob")).toContainText("security-admin");
  await expect(page.getByTestId("person-bob")).not.toContainText("viewer");
  await page.getByRole("button", { name: "Log out" }).click();
  await logIn(page, "bob");
  await tenancy(page, "Realms");
  await expect(page.getByTestId("tenancy-realm-acme")).toBeVisible();
  await expect(page.getByTestId("tenancy-pool-1")).toBeVisible();
  // Hosts and clients are theirs to bind; realms and pools aren't.
  await expect(page.getByRole("button", { name: "Bind a host" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Create realm" })).toHaveCount(0);
  await expect(page.getByTestId("tenancy-pool-1").getByRole("button", { name: "Change" })).toHaveCount(0);
  await page.getByRole("tab", { name: "Tenants & projects" }).click();
  await expect(page.getByTestId("tree-tenant-lab")).toBeVisible();
  await expect(page.getByTestId("tree-tenant-lab").getByRole("button", { name: "Realm", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Create tenant" })).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.getByRole("tab", { name: "Directories" }).click();
  await expect(page.getByTestId("directory-uni")).toBeVisible();
});

test("tenancy: ending a directory's trust unlinks its records", async ({ page }) => {
  await logIn(page, "ann");
  await tenancy(page, "Directories");
  await page.getByTestId("trusted-physics").getByRole("button", { name: "End trust" }).click();
  await expect(page.getByRole("dialog")).toContainText(/hyperstorctl admission end --directory \S+ --tenant physics/);
  await page.getByRole("dialog").getByRole("button", { name: "End trust" }).click();
  // Its last tenant: the directory is no longer trusted.
  await expect(page.getByTestId("directory-uni")).toHaveCount(0);
  await page.getByRole("tab", { name: "Tenants & projects" }).click();
  const physics = page.getByTestId("tree-tenant-physics");
  await expect(physics).not.toContainText("from directory");
  await expect(physics.getByRole("button", { name: "Rename" })).toBeVisible();
});
