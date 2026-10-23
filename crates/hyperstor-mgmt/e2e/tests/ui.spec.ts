// The web UI in a browser against a real cluster (global-setup.ts): the
// first administrator's claim, roles, the pages and their actions,
// tokens shown once, single sign-on, and logging out. In order: one
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
  await page.getByRole("link", { name: "Nodes" }).click();
  // Devices are numbered by node: 0, 1000, 2000.
  for (const d of [0, 1000, 2000]) await expect(page.getByTestId(`device-${d}`)).toContainText("in");
  // Out and back in, with the command shown.
  const dev = page.getByTestId("device-2000");
  await dev.getByRole("button", { name: "Take out" }).click();
  await expect(page.getByRole("dialog")).toContainText("hyperstorctl out --device 2000");
  await page.getByRole("dialog").getByRole("button", { name: "Take out" }).click();
  await expect(dev).toContainText("out");
  await dev.getByRole("button", { name: "Bring in" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Bring in" }).click();
  await expect(dev.getByText("in", { exact: true })).toBeVisible();
  await page.getByRole("link", { name: "Pools" }).click();
  await expect(page.getByTestId("pool-1")).toBeVisible();
  await page.getByTestId("pool-1").getByRole("button", { name: "PGs" }).click();
  await expect(page.getByText("1.0", { exact: true })).toBeVisible();
  await page.getByRole("link", { name: "Members" }).click();
  await expect(page.getByText("leader")).toBeVisible();
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
  // The clone goes, to the trash.
  await page.getByTestId("volume-web-clone").getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Move to the trash" }).click();
  await expect(page.getByTestId("volume-web-clone")).toHaveCount(0);
  await expect(page.getByText(/Moved to the trash: purged/)).toBeVisible();
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
  await expect(page.getByText("The trash is empty.")).toBeVisible();
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

test("someone without roles sees nothing to do", async ({ page }) => {
  await logIn(page, "bob");
  await expect(page.getByText("You have no roles yet")).toBeVisible();
  await page.getByRole("link", { name: "Volumes" }).click();
  await expect(page.getByRole("button", { name: "Create volume" })).toHaveCount(0);
  await page.getByRole("link", { name: "Security" }).click();
  await expect(page.getByRole("tab", { name: "People" })).toHaveCount(0);
  await page.getByRole("button", { name: "Log out" }).click();
  await expect(page.getByLabel("Username")).toBeVisible();
});

test("single sign-on with the identity provider", async ({ page }) => {
  await page.goto(`${url()}/volumes`);
  await page.getByRole("button", { name: /Sign in with/ }).click();
  await expect(page.getByTestId("whoami")).toContainText(process.env.HS_SSO!);
  // Back where the login began.
  await expect(page).toHaveURL(/\/volumes$/);
});
