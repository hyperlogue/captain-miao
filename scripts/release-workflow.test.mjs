import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
// Use the Nix-pinned YAML parser rather than duplicating the workflow in a test.
const parsed = spawnSync("yq", ["-o=json", ".", join(root, ".github/workflows/release.yml")], {
  encoding: "utf8",
});
assert.ifError(parsed.error);
assert.equal(parsed.status, 0, parsed.stderr);
const workflow = JSON.parse(parsed.stdout);
const launcher = "@hyperlogue/captain-miao";
const targets = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "linux-x64": "x86_64-unknown-linux-gnu",
};

// External writes are confined to this fake service state. Unknown commands
// fail, and workflow steps receive no ambient GitHub or npm credentials.
const fakeCommand = String.raw`#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const statePath = process.env.RELEASE_TEST_STATE;
const state = JSON.parse(fs.readFileSync(statePath, "utf8"));
const tool = path.basename(process.argv[1]);
const args = process.argv.slice(2);
function done(code = 0) {
  fs.writeFileSync(statePath, JSON.stringify(state));
  process.exit(code);
}
state.calls.push({ tool, args });
if (tool === "gh") {
  if (args[0] === "api") {
    if (state.networkFailure) done(1);
    const status = state.apiStatus ?? (state.release ? 200 : 404);
    const body = state.invalidResponse ? "invalid json" : JSON.stringify({ draft: state.draft ?? false });
    process.stdout.write("HTTP/2.0 " + status + "\r\nContent-Type: application/json\r\n\r\n" + body);
    done(status === 200 ? 0 : 1);
  }
  if (args[0] === "release" && args[1] === "create") {
    if (state.release) done(1);
    state.release = Object.fromEntries(args.filter(a => a.startsWith("artifacts/"))
      .map(a => [path.basename(a), fs.readFileSync(a).toString("base64")]));
    state.notes = fs.readFileSync(args[args.indexOf("--notes-file") + 1], "utf8");
    done();
  }
  if (args[0] === "release" && args[1] === "download") {
    if (!state.release) done(1);
    const dir = args[args.indexOf("--dir") + 1];
    const pattern = args[args.indexOf("--pattern") + 1];
    const [prefix, suffix] = pattern.split("*");
    fs.mkdirSync(dir, { recursive: true });
    for (const [name, bytes] of Object.entries(state.release)) {
      if (name !== state.missingAsset && name.startsWith(prefix) && name.endsWith(suffix)) {
        fs.writeFileSync(path.join(dir, name), Buffer.from(bytes, "base64"));
      }
    }
    done();
  }
} else if (tool === "npm") {
  if (args[0] === "install" || args[0] === "--version") done();
  if (args[0] === "view") {
    if (!args.includes("--prefer-online") || !(args[1] in state.published)) done(1);
    if (state.invisible && args.includes("--fetch-retries=0")) done(1);
    console.log(args[1].slice(args[1].lastIndexOf("@") + 1));
    done();
  }
  if (args[0] === "publish") {
    const pkg = JSON.parse(fs.readFileSync("package.json", "utf8"));
    const spec = pkg.name + "@" + pkg.version;
    state.attempts.push(spec);
    if (spec in state.published) done(1);
    if (pkg.optionalDependencies) {
      for (const [name, version] of Object.entries(pkg.optionalDependencies)) {
        if (!(name + "@" + version in state.published)) done(1);
      }
    }
    const fail = state.failPublish === pkg.name;
    if (!fail || state.commitBeforeFailure) {
      state.published[spec] = fs.existsSync("bin/miao") ? fs.readFileSync("bin/miao", "utf8") : "launcher";
    }
    if (fail) { delete state.failPublish; done(1); }
    done();
  }
}
console.error("Unexpected external command: " + tool + " " + args.join(" "));
done(1);
`;

function fixture(t, initial = {}, version = "1.2.3") {
  const dir = mkdtempSync(join(tmpdir(), "release-workflow-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const sub of ["bin", "npm", "scripts", "temp", "packages"]) mkdirSync(join(dir, sub));
  for (const script of ["stage-npm-packages.sh", "wait-for-npm-packages.sh"]) {
    copyFileSync(join(root, "scripts", script), join(dir, "scripts", script));
  }
  copyFileSync(join(root, "npm/package.json"), join(dir, "npm/package.json"));
  writeFileSync(join(dir, "Cargo.toml"), `[workspace.package]\nversion = "${version}"\n`);
  writeFileSync(join(dir, "README.md"), "Fixture readme.\n");
  writeFileSync(join(dir, "LICENSE"), "Fixture license.\n");
  writeFileSync(join(dir, "CHANGELOG.md"),
    `## [Unreleased]\nFuture.\n\n## [${version}]\n\nRelease notes.\n\n## [1.0.0]\nOld.\n\n[${version}]: example\n`);
  for (const tool of ["gh", "npm"]) writeFileSync(join(dir, "bin", tool), fakeCommand, { mode: 0o755 });

  const payload = (origin, platform) => `#!/usr/bin/env bash\n# ${origin} bytes for ${platform}\nexit 99\n`;
  function assets(origin) {
    const archives = {};
    for (const [platform, target] of Object.entries(targets)) {
      const name = `miao-v${version}-${target}`;
      const source = join(dir, "packages", name);
      mkdirSync(source, { recursive: true });
      writeFileSync(join(source, "miao"), payload(origin, platform), { mode: 0o755 });
      const packed = spawnSync("tar", ["-czf", "-", "-C", join(dir, "packages"), name]);
      assert.ifError(packed.error);
      assert.equal(packed.status, 0, packed.stderr.toString());
      archives[`${name}.tar.gz`] = packed.stdout.toString("base64");
    }
    // GitHub-only assets must be attached but never staged as npm dashboards.
    for (const prefix of ["miao-server", "miao-bundled-all-server"]) {
      archives[`${prefix}-v${version}-x86_64-unknown-linux-gnu.tar.gz`] = Buffer.from(origin).toString("base64");
    }
    return archives;
  }
  const built = assets("build");
  const published = assets("published");
  const statePath = join(dir, "state.json");
  writeFileSync(statePath, JSON.stringify({ release: null, published: {}, calls: [], attempts: [], ...initial,
    ...(initial.release ? { release: published } : {}),
  }));
  const state = () => JSON.parse(readFileSync(statePath, "utf8"));

  // Run the actual shell steps, staging script and visibility wait. Only action
  // setup and artifact downloads are simulated; conditions come from the YAML.
  function run(artifactExpired = false) {
    rmSync(join(dir, "artifacts"), { recursive: true, force: true });
    rmSync(join(dir, "dist"), { recursive: true, force: true });
    const output = join(dir, "temp/output");
    writeFileSync(output, "");
    for (const step of workflow.jobs.publish.steps) {
      if (step.if) {
        assert.equal(step.if, "steps.release.outputs.exists == 'false'");
        if (!readFileSync(output, "utf8").includes("exists=false")) continue;
      }
      if (step.uses) {
        if (step.uses.startsWith("actions/download-artifact@")) {
          if (artifactExpired) return { failed: "Download build artifacts" };
          const dest = join(dir, step.with.path);
          mkdirSync(dest, { recursive: true });
          for (const [name, bytes] of Object.entries(built)) {
            writeFileSync(join(dest, name), Buffer.from(bytes, "base64"));
          }
        } else {
          assert.match(step.uses, /^actions\/(checkout|setup-node)@/);
        }
        continue;
      }
      assert.equal(typeof step.run, "string");
      const result = spawnSync("bash", ["--noprofile", "--norc", "-eo", "pipefail", "-c", step.run], {
        cwd: join(dir, step["working-directory"] ?? "."),
        encoding: "utf8",
        timeout: 15_000,
        env: {
          PATH: [join(dir, "bin"), dirname(process.execPath), process.env.PATH].join(delimiter),
          RELEASE_TEST_STATE: statePath,
          VERSION: version,
          EXPECT_VERSION: version,
          GITHUB_REPOSITORY: "example/project",
          GH_REPO: "example/project",
          GITHUB_OUTPUT: output,
          RUNNER_TEMP: join(dir, "temp"),
          NPM_VISIBILITY_TIMEOUT_SECONDS: initial.invisible ? "1" : "10",
        },
      });
      assert.ifError(result.error);
      if (result.status !== 0) return { failed: step.name, output: result.stdout + result.stderr };
    }
    return { failed: undefined };
  }
  return { dir, state, run, built, published, payload };
}

const succeeded = (result) => assert.equal(result.failed, undefined, JSON.stringify(result));
const releaseWrites = (state) => state.calls.filter(({ tool, args }) =>
  tool === "gh" && args[0] === "release" && ["create", "edit", "upload"].includes(args[1]));

test("one environment approval covers GitHub and npm publication", () => {
  assert.deepEqual(Object.entries(workflow.jobs).filter(([, job]) => job.environment).map(([name]) => name), ["publish"]);
  assert.equal(workflow.jobs.publish.environment, "release");
  assert.deepEqual(workflow.permissions, { contents: "read" });
  assert.deepEqual(workflow.jobs.publish.permissions, { contents: "write", "id-token": "write" });
  assert.deepEqual(workflow.jobs.publish.needs, ["verify", "build"]);
  for (const value of Object.values(workflow.jobs.publish.env)) {
    assert.equal(value, "${{ needs.verify.outputs.version }}");
  }
  for (const job of Object.values(workflow.jobs)) {
    for (const step of job.steps ?? []) assert.ok(!step.run?.includes("${{"));
  }
  const setup = workflow.jobs.publish.steps.filter(step => step.uses?.startsWith("actions/setup-node@"));
  assert.equal(setup.length, 1);
  assert.equal(setup[0].with["registry-url"], undefined);
});

for (const commitBeforeFailure of [false, true]) {
  test(`a partial npm failure resumes after artifacts expire (registry accepted: ${commitBeforeFailure})`, (t) => {
    const f = fixture(t, { failPublish: `${launcher}-darwin-x64`, commitBeforeFailure });
    assert.equal(f.run().failed, "Publish platform binary packages");
    const first = f.state();
    assert.equal(Object.keys(first.published).length, commitBeforeFailure ? 2 : 1);
    assert.deepEqual(first.release, f.built);
    succeeded(f.run(true));
    const retried = f.state();
    assert.deepEqual(retried.release, first.release);
    assert.equal(releaseWrites(retried).length, 1);
    for (const platform of Object.keys(targets)) {
      assert.equal(retried.published[`${launcher}-${platform}@1.2.3`], f.payload("build", platform));
    }
    assert.equal(Object.keys(retried.published).at(-1), `${launcher}@1.2.3`);
    succeeded(f.run(true));
    assert.deepEqual(f.state().attempts, retried.attempts);
    assert.equal(releaseWrites(f.state()).length, 1);
  });
}

test("an existing release supplies npm bytes without downloading build artifacts", (t) => {
  const f = fixture(t, { release: true });
  succeeded(f.run(true));
  assert.deepEqual(f.state().release, f.published);
  assert.deepEqual(releaseWrites(f.state()), []);
  for (const platform of Object.keys(targets)) {
    assert.equal(f.state().published[`${launcher}-${platform}@1.2.3`], f.payload("published", platform));
  }
});

test("a launcher failure retries without republishing platforms", (t) => {
  const f = fixture(t, { release: true, failPublish: launcher });
  assert.equal(f.run(true).failed, "Publish npm launcher");
  succeeded(f.run(true));
  assert.equal(f.state().attempts.filter(spec => spec.startsWith(`${launcher}-`)).length, 4);
  assert.equal(Object.keys(f.state().published).length, 5);
  assert.deepEqual(releaseWrites(f.state()), []);
});

for (const scenario of [{ apiStatus: 403 }, { apiStatus: 500 }, { networkFailure: true }, { apiStatus: 200, invalidResponse: true }]) {
  test(`a failed GitHub lookup stops before artifact download: ${JSON.stringify(scenario)}`, (t) => {
    const f = fixture(t, scenario);
    assert.equal(f.run(true).failed, "Check for an existing GitHub Release");
    assert.equal(f.state().calls.length, 1);
    assert.deepEqual(f.state().published, {});
  });
}

test("an unfinished draft stops publication", (t) => {
  const f = fixture(t, { release: true, draft: true });
  assert.equal(f.run(true).failed, "Check for an existing GitHub Release");
  assert.deepEqual(f.state().published, {});
  assert.deepEqual(releaseWrites(f.state()), []);
});

test("a missing published asset cannot use leftover build bytes", (t) => {
  const f = fixture(t, { missingAsset: "miao-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" });
  assert.equal(f.run().failed, "Stage npm packages and launcher pins");
  assert.deepEqual(f.state().published, {});
});

test("an initial publication still requires build artifacts", (t) => {
  const f = fixture(t);
  assert.equal(f.run(true).failed, "Download build artifacts");
  assert.deepEqual(releaseWrites(f.state()), []);
});

test("the launcher stays unpublished if platform visibility times out", (t) => {
  const f = fixture(t, { release: true, invisible: true });
  assert.equal(f.run(true).failed, "Wait for platform packages to be visible");
  assert.equal(Object.keys(f.state().published).length, 4);
  assert.ok(!f.state().attempts.includes(`${launcher}@1.2.3`));
});

for (const version of ["1.2.3", "1.2.3-rc.1"]) {
  test(`release notes and publication flags are preserved for ${version}`, (t) => {
    const f = fixture(t, {}, version);
    succeeded(f.run());
    const { args } = releaseWrites(f.state())[0];
    assert.equal(args[2], `v${version}`);
    assert.ok(args.includes("--verify-tag"));
    assert.ok(args.includes("--generate-notes"));
    assert.equal(args.includes("--prerelease"), version.includes("-"));
    assert.equal(f.state().notes.trim(), "Release notes.");
    const publishes = f.state().calls.filter(call => call.tool === "npm" && call.args[0] === "publish");
    assert.equal(publishes.length, 5);
    for (const call of publishes) {
      assert.ok(call.args.includes("--ignore-scripts"));
      assert.ok(call.args.includes("--provenance"));
    }
    assert.deepEqual(publishes.at(-1).args.slice(-2), ["--tag", version.includes("-") ? "next" : "latest"]);
  });
}
