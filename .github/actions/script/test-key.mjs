// The real key program against isolated checkouts; no compiler, network or repository writes.
// Source changes must change a key, incomplete inputs must produce no key, and planted old
// omissions must break those oracles. CI runs this once in scripts, not on every cache hit.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

assert.equal(Bun.version, "1.4.5", "the fixture suite uses the pinned TOML parser");
const started = performance.now();
const temporary = mkdtempSync(join(tmpdir(), "blaze-script-key-test-"));
const base = join(temporary, "base");
const checkout = join(temporary, "checkout");
const action = dirname(fileURLToPath(import.meta.url));
const environment = { ...process.env };
let checks = 0;

function write(path, content, directory = checkout) {
  const destination = join(directory, path);
  mkdirSync(dirname(destination), { recursive: true });
  writeFileSync(destination, content);
}
function read(path) { return readFileSync(join(checkout, path), "utf8"); }
function replace(path, before, after) {
  const original = read(path);
  assert(original.includes(before), `mutation needs ${before} in ${path}`);
  const changed = original.replace(before, after);
  assert.notEqual(changed, original, `mutation must change ${path}`);
  write(path, changed);
}
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { cwd: checkout, env: environment, encoding: "utf8", timeout: 10_000, ...options });
  if (result.error) throw result.error;
  assert.notEqual(result.status, null, `${command} exited by signal ${result.signal}`);
  return result;
}
function key(env = environment) {
  return run("bash", [".github/actions/script/key.sh"], { env });
}
function successful(result) {
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /^scripts-[^\r\n]+-[0-9a-f]{64}\n$/);
  return result.stdout;
}
function reset() {
  rmSync(checkout, { recursive: true, force: true });
  cpSync(base, checkout, { recursive: true });
}
function check(label, test) {
  reset();
  test();
  checks++;
  console.log(`ok ${checks}: ${label}`);
}
function changes(label, mutate) {
  check(label, () => { mutate(); assert.notEqual(successful(key()), baseline, label); });
}
function refuses(label, mutate, diagnostic) {
  check(label, () => {
    const env = mutate() ?? environment;
    const result = key(env);
    assert.notEqual(result.status, 0, `${label}: unexpectedly accepted`);
    assert.equal(result.stdout, "", `${label}: partial successful output`);
    assert.match(result.stderr, diagnostic);
  });
}
function decoy(packageName) {
  write(`decoy/${packageName}/Cargo.toml`, `[package]\nname = "${packageName}"\nversion = "1.0.0"\n`);
  write(`decoy/${packageName}/src/lib.rs`, "pub const DECOY: bool = true;\n");
  write("Cargo.toml", read("Cargo.toml") + `\n[patch.crates-io]\n${packageName} = { path = "decoy/${packageName}" }\n`);
}
function sourceTable(header) {
  write("crates/shared/Cargo.toml", `[package]\nname = "shared"\nversion = "1.0.0"\n${header}\ndeep = { path = "../../nested/deep" }\n`);
}
function mock(command, body) {
  const bin = join(temporary, "mock-bin");
  rmSync(bin, { recursive: true, force: true });
  mkdirSync(bin);
  const path = join(bin, command);
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
  return { ...environment, PATH: `${bin}:${environment.PATH}` };
}

let baseline;
try {
  mkdirSync(base);
  cpSync(action, join(base, ".github/actions/script"), { recursive: true });
  const fixture = {
    ".github/blaze-version": "0.test.1\n",
    "pins/toolchain": "nightly-test\n",
    "pins/llvm.json": '{"revision":"fixture-one"}\n',
    ".cargo/config.toml": "[env]\nTEST_INPUT = \"one\"\n",
    "blaze.lock": "version = 1\n",
    "Cargo.toml": `[workspace]
members = ["scripts", "crates/*"]
[workspace.package]
version = "1.0.0"
edition = "2024"
[workspace.dependencies]
direct = { path = "crates/direct" }
shared = { path = "crates/shared" }
vendor_alias = { package = "vendor-leaf", path = "vendor/leaf" }
registry-pkg = "1.0.0"
`,
    "scripts/Cargo.toml": `[package]
name = "scripts"
version.workspace = true
[dependencies]
direct.workspace = true
registry-pkg.workspace = true
`,
    "crates/direct/Cargo.toml": `[package]
name = "direct"
version.workspace = true
[dependencies]
shared.workspace = true
vendor_alias.workspace = true
`,
    "crates/shared/Cargo.toml": `[package]
name = "shared"
version = "1.0.0"
[dependencies]
deep = { path = "../../nested/deep" }
`,
    "nested/Cargo.toml": `[workspace]
members = ["deep"]
[workspace.package]
version = "1.0.0"
edition = "2024"
`,
    "nested/deep/Cargo.toml": `[package]
name = "deep"
version.workspace = true
edition.workspace = true
`,
    "vendor/leaf/Cargo.toml": `[package]
name = "vendor-leaf"
version = "1.0.0"
[workspace]
`,
    // A malformed unrelated fixture must not impersonate a real local path or block it.
    "unrelated/Cargo.toml": "intentionally invalid TOML\n",
    "Cargo.lock": `version = 4

[[package]]
name = "scripts"
version = "1.0.0"
dependencies = [
 "direct",
 "registry-pkg",
]

[[package]]
name = "direct"
version = "1.0.0"
dependencies = [
 "shared",
 "vendor-leaf",
]

[[package]]
name = "shared"
version = "1.0.0"
dependencies = [
 "deep",
]

[[package]]
name = "deep"
version = "1.0.0"

[[package]]
name = "vendor-leaf"
version = "1.0.0"

[[package]]
name = "registry-pkg"
version = "1.0.0"
source = "registry+https://one.invalid/index"
checksum = "${"a".repeat(64)}"

[[package]]
name = "unused"
version = "1.0.0"
source = "registry+https://one.invalid/index"
checksum = "${"c".repeat(64)}"

[[patch.unused]]
name = "unused-patch"
version = "1.0.0"
`,
  };
  for (const [path, content] of Object.entries(fixture)) write(path, content, base);
  for (const path of ["scripts/src/main.rs", "crates/direct/src/lib.rs", "crates/shared/src/lib.rs", "nested/deep/src/lib.rs", "vendor/leaf/src/lib.rs", "vendor/leaf/c/native.c"]) {
    write(path, "// initial source\n", base);
  }
  assert.equal(run("git", ["init", "-q"], { cwd: base }).status, 0);
  assert.equal(run("git", ["add", "."], { cwd: base }).status, 0);
  reset();
  baseline = successful(key());
  check("stable key and unused patch record", () => assert.equal(successful(key()), baseline));
  for (const path of ["scripts/src/main.rs", "crates/direct/src/lib.rs", "crates/shared/src/lib.rs", "nested/deep/src/lib.rs", "vendor/leaf/c/native.c"]) {
    changes(`source input ${path}`, () => write(path, "// changed source\n"));
  }
  changes("whole root workspace manifest", () => replace("Cargo.toml", 'registry-pkg = "1.0.0"', 'registry-pkg = "=1.0.0"'));
  changes("blaze lock input", () => write("blaze.lock", "version = 2\n"));
  changes("pinned Cargo toolchain", () => write("pins/toolchain", "nightly-other\n"));
  changes("pinned LLVM recipe", () => write("pins/llvm.json", '{"revision":"fixture-two"}\n'));
  changes("selected package manifest", () => write("crates/shared/Cargo.toml", read("crates/shared/Cargo.toml") + "# package input\n"));
  changes("ancestor workspace manifest", () => replace("nested/Cargo.toml", 'edition = "2024"', 'edition = "2021"'));
  check("literal identity still hashes ignored ancestor workspace", () => {
    replace("nested/deep/Cargo.toml", "version.workspace = true", 'version = "1.0.0"');
    assert.equal(run("git", ["rm", "--cached", "nested/Cargo.toml"]).status, 0);
    write(".gitignore", "nested/Cargo.toml\n");
    const before = successful(key());
    replace("nested/Cargo.toml", 'edition = "2024"', 'edition = "2021"');
    assert.notEqual(successful(key()), before);
  });
  changes("source addition", () => write("crates/shared/data/input.txt", "new build input\n"));
  changes("source deletion", () => rmSync(join(checkout, "crates/shared/src/lib.rs")));
  changes("reachable registry checksum", () => replace("Cargo.lock", "a".repeat(64), "b".repeat(64)));
  check("unreachable lock block and unrelated fixture stay outside closure", () => {
    replace("Cargo.lock", "c".repeat(64), "d".repeat(64));
    rmSync(join(checkout, "unrelated/Cargo.toml"));
    assert.equal(successful(key()), baseline);
  });
  refuses("missing local source cannot borrow a decoy identity", () => {
    assert.equal(run("git", ["rm", "--cached", "-r", "nested/deep"]).status, 0);
    write(".gitignore", "nested/deep/\n");
    rmSync(join(checkout, "nested/deep"), { recursive: true });
    decoy("deep");
  }, /nested\/deep/);
  refuses("external path cannot borrow an in-tree decoy", () => {
    decoy("deep");
    replace("crates/shared/Cargo.toml", "../../nested/deep", join(temporary, "outside"));
  }, /escapes checkout/);
  refuses("declared package name must match its source", () => replace("nested/deep/Cargo.toml", 'name = "deep"', 'name = "wrong"'), /actually wrong/);
  refuses("source symlink", () => symlinkSync("lib.rs", join(checkout, "crates/shared/src/alias.rs")), /symlinked source/);
  refuses("package directory symlink", () => {
    cpSync(join(checkout, "nested/deep"), join(checkout, "real-deep"), { recursive: true });
    rmSync(join(checkout, "nested/deep"), { recursive: true });
    symlinkSync("../real-deep", join(checkout, "nested/deep"));
  }, /symlinked input/);
  refuses("sparse checkout", () => { assert.equal(run("git", ["config", "core.sparseCheckout", "true"]).status, 0); }, /sparse checkout/);
  refuses("missing pinned toolchain", () => rmSync(join(checkout, "pins/toolchain")), /missing or symlinked input pins\/toolchain/);
  refuses("failing hash command cannot print a key", () => mock("sha256sum", "exit 23"), /cannot hash source input/);
  refuses("wrong TOML runtime cannot use a fallback", () => mock("bun", "echo 0.0.0"), /Bun 1\.4\.5 is required/);
  refuses("unhashable dependency after valid closure blocks", () => replace("Cargo.lock", `checksum = "${"a".repeat(64)}"\n`, ""), /unhashable dependency registry-pkg/);
  refuses("unsupported git dependency", () => replace("Cargo.lock", "registry+https://one.invalid/index", "git+https://one.invalid/repo#123"), /unhashable dependency registry-pkg/);
  refuses("missing requested version does not fall back to its name", () => replace("Cargo.lock", ' "direct",', ' "direct 9.0.0",'), /missing dependency identity: direct 9\.0\.0/);
  refuses("missing lock package", () => {
    const blocks = read("Cargo.lock").split(/\n(?=\[\[)/);
    const kept = blocks.filter((block) => !block.startsWith('[[package]]\nname = "deep"\n'));
    assert.equal(kept.length, blocks.length - 1);
    write("Cargo.lock", kept.join("\n"));
  }, /missing dependency identity: deep/);
  const secondSource = `[[package]]\nname = "registry-pkg"\nversion = "1.0.0"\nsource = "registry+https://two.invalid/index"\nchecksum = "${"e".repeat(64)}"\n\n`;
  refuses("same name/version from two sources needs qualification", () => replace("Cargo.lock", "[[patch.unused]]", secondSource + "[[patch.unused]]"), /ambiguous dependency identity/);
  check("qualified source selects exactly its checksum", () => {
    replace("Cargo.lock", "[[patch.unused]]", secondSource + "[[patch.unused]]");
    replace("Cargo.lock", ' "registry-pkg",', ' "registry-pkg 1.0.0 (registry+https://one.invalid/index)",');
    const before = successful(key());
    replace("Cargo.lock", "e".repeat(64), "f".repeat(64));
    assert.equal(successful(key()), before);
    replace("Cargo.lock", "a".repeat(64), "b".repeat(64));
    assert.notEqual(successful(key()), before);
  });
  refuses("missing qualified source", () => replace("Cargo.lock", ' "registry-pkg",', ' "registry-pkg 1.0.0 (registry+https://missing.invalid/index)",'), /missing dependency identity/);
  refuses("malformed selected TOML", () => write("nested/deep/Cargo.toml", '[package\nname = "deep"\n'), /nested\/deep\/Cargo.toml/);
  for (const [label, mutate] of [
    ["header comment without whitespace", () => replace("crates/shared/Cargo.toml", "[package]", "[package]#comment")],
    ["CRLF package and inherited workspace", () => { write("crates/shared/Cargo.toml", read("crates/shared/Cargo.toml").replaceAll("\n", "\r\n")); write("Cargo.toml", read("Cargo.toml").replaceAll("\n", "\r\n")); }],
    ["dotted package identity", () => replace("crates/shared/Cargo.toml", '[package]\nname = "shared"\nversion = "1.0.0"', 'package.name = "shared"\npackage.version = "1.0.0"')],
    ["inline package identity", () => replace("crates/shared/Cargo.toml", '[package]\nname = "shared"\nversion = "1.0.0"', 'package = { name = "shared", version = "1.0.0" }')],
    ["quoted package header", () => replace("crates/shared/Cargo.toml", "[package]", '["package"]')],
  ]) {
    refuses(`${label} cannot hide the actual source`, () => { mutate(); decoy("shared"); }, /ambiguous declared local package/);
  }
  for (const [label, mutate] of [
    ["spaced dependency header", () => sourceTable("[ dependencies ]")],
    ["quoted target dependency header", () => sourceTable('[target.\'cfg(unix)\'."dependencies"]')],
    ["inline workspace dependencies", () => write("crates/shared/Cargo.toml", '[package]\nname = "shared"\nversion = "1.0.0"\n[workspace]\ndependencies = { deep = { path = "../../nested/deep" } }\n[dependencies]\ndeep.workspace = true\n')],
    ["inline target dependencies", () => sourceTable('[target]\n\'cfg(unix)\' = { dependencies = { deep = { path = "../../nested/deep" } } }\n[metadata]')],
  ]) {
    refuses(`${label} cannot hide a path behind a patch decoy`, () => { mutate(); decoy("deep"); }, /ambiguous declared local package/);
  }
  refuses("explicit workspace is refused even with literal version", () => replace("crates/shared/Cargo.toml", 'version = "1.0.0"', 'version = "1.0.0"\nworkspace = "../.."'), /explicit package.workspace/);
  refuses("path base cannot select a package-relative decoy", () => {
    replace("Cargo.toml", 'direct = { path = "crates/direct" }\n', "");
    replace("scripts/Cargo.toml", "direct.workspace = true", 'direct = { path = "dep", base = "workspace" }');
    cpSync(join(checkout, "crates/direct"), join(checkout, "dep"), { recursive: true });
    cpSync(join(checkout, "crates/direct"), join(checkout, "scripts/dep"), { recursive: true });
  }, /unsupported dependency path base/);
  refuses("replace table is not silently ignored", () => write("Cargo.toml", read("Cargo.toml") + '\n[replace]\n"deep:1.0.0" = { path = "nested/deep" }\n'), /replace tables are unsupported/);
  for (const content of ['paths = ["other"]\n', '[source.crates-io]\nreplace-with = "local"\n[source.local]\ndirectory = "other"\n', 'include = ["other.toml"]\n']) {
    refuses(`Cargo source override ${content.split("\n")[0]}`, () => write(".cargo/config.toml", content), /source overrides are unsupported/);
  }
  refuses("legacy config is not silently ignored", () => write(".cargo/config", 'paths = ["other"]\n'), /legacy .cargo\/config/);
  check("key construction never invokes a compiler", () => {
    const marker = join(temporary, "compiler-ran");
    const env = mock("blaze-engine", `echo ran > '${marker}'; exit 99`);
    const bin = env.PATH.split(":")[0];
    for (const command of ["blaze", "cargo", "rustc"]) cpSync(join(bin, "blaze-engine"), join(bin, command));
    assert.equal(successful(key(env)), baseline);
    assert(!existsSync(marker));
  });
  check("source-coverage oracle detects a planted transitive omission", () => {
    replace(".github/actions/script/key.sh", "while IFS= read -r dir; do\n", 'while IFS= read -r dir; do\n  [ "$dir" = crates/shared ] && continue\n');
    const planted = successful(key());
    write("crates/shared/src/lib.rs", "// source omitted by the planted bug\n");
    assert.equal(successful(key()), planted, "the planted omission must break the source-change oracle");
  });
  check("actual action propagates failure; old echo wrapper fails the oracle", () => {
    replace("Cargo.lock", `checksum = "${"a".repeat(64)}"\n`, "");
    const output = join(temporary, "github-output");
    const env = { ...environment, GITHUB_ACTION_PATH: join(checkout, ".github/actions/script"), GITHUB_OUTPUT: output, RUNNER_TEMP: temporary };
    const step = read(".github/actions/script/action.yml").match(/    - id: key\n([\s\S]*?)(?=\n    - )/);
    assert(step, "actual composite key step exists");
    const body = step[1].match(/      run: \|\n([\s\S]*)/);
    assert(body, "actual key step is a shell block");
    const code = body[1].split("\n").map((line) => line.replace(/^        /, "")).join("\n");
    const actual = run("bash", ["-euo", "pipefail", "-c", code], { env });
    assert.notEqual(actual.status, 0, actual.stderr);
    assert(!existsSync(output) || readFileSync(output, "utf8") === "", "failed key must not enter restore outputs");
    const old = run("bash", ["-euo", "pipefail", "-c", 'echo "key=$("$GITHUB_ACTION_PATH/key.sh")" >> "$GITHUB_OUTPUT"'], { env });
    assert.equal(old.status, 0, "the planted old wrapper masks the failure");
    assert.equal(readFileSync(output, "utf8"), "key=\n", "the planted wrapper violates the refusal oracle");
  });
  console.log(`${checks} script cache-key controls passed in ${((performance.now() - started) / 1000).toFixed(2)}s; no compiler invoked`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
