import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const script = fileURLToPath(new URL("./wait-for-npm-packages.sh", import.meta.url));
const packages = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"]
  .map((platform) => `@example/dashboard-${platform}`);

function run(t, scenario = {}, timeoutSeconds = "600") {
  const dir = mkdtempSync(join(tmpdir(), "npm-visibility-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const bin = join(dir, "bin");
  mkdirSync(bin);
  writeFileSync(join(dir, "package.json"), JSON.stringify({
    optionalDependencies: Object.fromEntries(packages.map((name) => [name, "1.2.3"])),
  }));
  writeFileSync(join(dir, "scenario.json"), JSON.stringify(scenario));
  writeFileSync(join(bin, "sleep"), "#!/usr/bin/env bash\nexit 0\n", { mode: 0o755 });
  writeFileSync(join(bin, "npm"), `#!/usr/bin/env node
const fs = require("node:fs");
const scenario = JSON.parse(fs.readFileSync("scenario.json", "utf8"));
const spec = process.argv[3];
const state = fs.existsSync("state.json")
  ? JSON.parse(fs.readFileSync("state.json", "utf8")) : {};
state[spec] = (state[spec] || 0) + 1;
fs.writeFileSync("state.json", JSON.stringify(state));
// A stale local cache must not mask a now-visible registry version.
if (!process.argv.includes("--prefer-online")) process.exit(1);
if (scenario.hang) {
  setTimeout(() => process.exit(1), 60000);
} else if (scenario.missing || state[spec] <= (scenario.misses?.[spec] || 0)) {
  process.exit(1);
} else {
  console.log(scenario.wrongVersion ? "1.2.2" : "1.2.3");
  process.exit(scenario.failedResponse ? 1 : 0);
}
`, { mode: 0o755 });
  const result = spawnSync("bash", [script, "package.json"], {
    cwd: dir,
    encoding: "utf8",
    timeout: 15000,
    env: {
      ...process.env,
      PATH: `${bin}${delimiter}${process.env.PATH}`,
      NPM_VISIBILITY_TIMEOUT_SECONDS: timeoutSeconds,
    },
  });
  assert.ifError(result.error);
  const counts = (() => {
    try { return JSON.parse(readFileSync(join(dir, "state.json"), "utf8")); }
    catch { return {}; }
  })();
  return { ...result, counts };
}

test("waits beyond the old 30-poll limit and checks every platform", (t) => {
  const slow = `${packages[1]}@1.2.3`;
  const result = run(t, { misses: { [slow]: 35 } });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.equal(result.counts[slow], 36);
  for (const name of packages) assert.match(result.stdout, new RegExp(`${name}@1.2.3 visible`));
});

test("missing packages fail at the deadline", (t) => {
  const result = run(t, { missing: true }, "1");
  assert.equal(result.status, 1);
  assert.match(result.stdout, /not visible within 1s/);
  assert.match(result.stdout, /Re-run failed jobs/);
});

test("a stalled npm request cannot exceed the deadline", (t) => {
  const result = run(t, { hang: true }, "1");
  assert.equal(result.status, 1);
  assert.match(result.stdout, /not visible within 1s/);
});

test("another version does not satisfy the launcher's exact pin", (t) => {
  const result = run(t, { wrongVersion: true }, "1");
  assert.equal(result.status, 1);
  assert.doesNotMatch(result.stdout, /1\.2\.3 visible/);
});

test("an unsuccessful npm response cannot satisfy the visibility check", (t) => {
  const result = run(t, { failedResponse: true }, "1");
  assert.equal(result.status, 1);
  assert.doesNotMatch(result.stdout, /1\.2\.3 visible/);
});

test("rejects an invalid deadline before querying the registry", (t) => {
  const result = run(t, {}, "0");
  assert.equal(result.status, 1);
  assert.match(result.stderr, /must be a positive integer/);
  assert.deepEqual(result.counts, {});
});
