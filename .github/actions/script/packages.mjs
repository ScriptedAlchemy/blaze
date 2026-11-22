// Resolve local lock identities through Cargo's declared paths, never a same-name directory
// search. Bun parses TOML; spelling, quoting, inline tables and comments cannot hide a path.
// key.sh hashes these package trees and every manifest read for workspace inheritance.
import { existsSync, lstatSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";

const root = realpathSync(process.cwd());
const manifests = new Map();
const sources = new Map();
const queue = [];
const kinds = ["dependencies", "build-dependencies", "dev-dependencies"];
const fail = (message) => { throw new Error(message); };
const table = (value, label) => {
  if (Object.prototype.toString.call(value) !== "[object Object]") fail(`${label}: expected a table`);
  return value;
};
const name = (value, label) => {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]+$/.test(value)) fail(`${label}: invalid package name`);
  return value;
};

function checkedPath(path) {
  const absolute = resolve(root, path);
  const local = relative(root, absolute) || ".";
  if (isAbsolute(local) || local === ".." || local.startsWith("../")) fail(`dependency path escapes checkout: ${path}`);
  if (/[\r\n\t\0]/.test(local)) fail(`unsupported input path: ${path}`);
  if (realpathSync(absolute) !== absolute) fail(`symlinked input: ${path}`);
  return local;
}

function parse(path) {
  const local = checkedPath(path);
  if (!lstatSync(resolve(root, local)).isFile()) fail(`not a regular manifest: ${local}`);
  try {
    return table(Bun.TOML.parse(readFileSync(resolve(root, local), "utf8")), local);
  } catch (error) {
    fail(`${local}: ${error.message}`);
  }
}

function manifest(path) {
  const local = checkedPath(path);
  if (!manifests.has(local)) {
    const data = parse(local);
    if (Object.hasOwn(data, "replace")) fail(`${local}: Cargo replace tables are unsupported by the source key`);
    manifests.set(local, { path: local, data });
  }
  return manifests.get(local);
}

function workspace(owner) {
  let dir = dirname(owner.path);
  while (true) {
    const path = dir === "." ? "Cargo.toml" : `${dir}/Cargo.toml`;
    // lstat also sees a dangling symlink, which manifest() must refuse rather than skip.
    let present;
    try { lstatSync(resolve(root, path)); present = true; }
    catch (error) { if (error.code !== "ENOENT") throw error; present = false; }
    if (present) {
      const candidate = manifest(path);
      if (Object.hasOwn(candidate.data, "workspace")) {
        table(candidate.data.workspace, `${path}: workspace`);
        return candidate;
      }
    }
    if (dir === ".") fail(`${owner.path}: no ancestor workspace for inheritance`);
    dir = dirname(dir);
  }
}

function identity(owner) {
  const pkg = table(owner.data.package, `${owner.path}: package`);
  const packageName = name(pkg.name, `${owner.path}: package.name`);
  if (Object.hasOwn(pkg, "workspace")) fail(`${owner.path}: explicit package.workspace is unsupported by the source key`);
  let version = pkg.version;
  if (typeof version !== "string") {
    const inherited = table(version, `${owner.path}: package.version`);
    if (inherited.workspace !== true || Object.keys(inherited).length !== 1) fail(`${owner.path}: unsupported inherited package.version`);
    const parent = workspace(owner);
    version = table(parent.data.workspace.package, `${parent.path}: workspace.package`).version;
  }
  if (typeof version !== "string" || !/^[0-9][A-Za-z0-9.+_-]*$/.test(version)) fail(`${owner.path}: invalid package.version`);
  return { name: packageName, key: `${packageName}\t${version}` };
}

function select(path, expectedName) {
  const owner = manifest(path);
  const actual = identity(owner);
  // Literal versions can still inherit edition, profiles or workspace dependencies. Read
  // the ancestor workspace even when identity() did not need its package.version.
  workspace(owner);
  if (actual.name !== expectedName) fail(`${owner.path}: declared package ${expectedName} is actually ${actual.name}`);
  if (!wanted.has(actual.key)) fail(`${owner.path}: local identity is absent from the scripts lock closure: ${actual.key}`);
  const previous = sources.get(actual.key);
  if (previous && previous.path !== owner.path) fail(`ambiguous declared local package: ${actual.key}: ${previous.path}, ${owner.path}`);
  if (!previous) {
    sources.set(actual.key, owner);
    queue.push(owner);
  }
}

function dependency(alias, value, owner, inherited = false) {
  if (typeof value === "string") return;
  const spec = table(value, `${owner.path}: dependency ${alias}`);
  if (Object.hasOwn(spec, "workspace")) {
    if (inherited || spec.workspace !== true || ["path", "package", "git", "registry", "version"].some((key) => Object.hasOwn(spec, key))) {
      fail(`${owner.path}: unsupported workspace dependency ${alias}`);
    }
    const parent = workspace(owner);
    const definitions = table(parent.data.workspace.dependencies, `${parent.path}: workspace.dependencies`);
    if (!Object.hasOwn(definitions, alias)) fail(`${owner.path}: missing workspace dependency ${alias}`);
    dependency(alias, definitions[alias], parent, true);
    return;
  }
  const packageName = name(spec.package ?? alias, `${owner.path}: dependency ${alias}`);
  // Root manifests also declare parked packages outside this lock closure. Their manifests
  // need not exist; a declaration for a needed name, however, can never disappear in favour
  // of a same-name patch or another path elsewhere in the checkout.
  if (!wantedNames.has(packageName) || !Object.hasOwn(spec, "path")) return;
  if (Object.hasOwn(spec, "base")) fail(`${owner.path}: unsupported dependency path base for ${alias}`);
  if (typeof spec.path !== "string" || spec.path.length === 0) fail(`${owner.path}: invalid path for ${alias}`);
  if (Object.hasOwn(spec, "git") || Object.hasOwn(spec, "registry")) fail(`${owner.path}: multiple source kinds for ${alias}`);
  select(resolve(root, dirname(owner.path), spec.path, "Cargo.toml"), packageName);
}

function dependencies(owner, data) {
  for (const kind of kinds) {
    if (!Object.hasOwn(data, kind)) continue;
    for (const [alias, value] of Object.entries(table(data[kind], `${owner.path}: ${kind}`))) {
      dependency(alias, value, owner);
    }
  }
}

try {
  if (process.argv.length !== 4) fail("usage: packages.mjs LOCAL_IDENTITIES USED_MANIFESTS");
  const rows = readFileSync(process.argv[2], "utf8").trimEnd().split("\n");
  var wanted = new Set();
  var wantedNames = new Set();
  for (const row of rows) {
    if (!/^[A-Za-z0-9_-]+\t[0-9][A-Za-z0-9.+_-]*$/.test(row) || wanted.has(row)) fail("invalid local identity list from lock parser");
    wanted.add(row);
    wantedNames.add(row.split("\t")[0]);
  }
  if (existsSync(".cargo/config")) fail("legacy .cargo/config is unsupported by the source key");
  const config = parse(".cargo/config.toml");
  if (["paths", "source", "patch", "replace", "include"].some((key) => Object.hasOwn(config, key))) {
    fail(".cargo/config.toml: Cargo source overrides are unsupported by the source key");
  }
  const top = manifest("Cargo.toml");
  select("scripts/Cargo.toml", "scripts");
  if (Object.hasOwn(top.data, "workspace")) {
    const shared = table(top.data.workspace, "Cargo.toml: workspace").dependencies;
    if (shared !== undefined) {
      for (const [alias, value] of Object.entries(table(shared, "Cargo.toml: workspace.dependencies"))) {
        dependency(alias, value, top, true);
      }
    }
  }
  if (Object.hasOwn(top.data, "patch")) {
    for (const [source, patches] of Object.entries(table(top.data.patch, "Cargo.toml: patch"))) {
      for (const [alias, value] of Object.entries(table(patches, `Cargo.toml: patch.${source}`))) {
        dependency(alias, value, top);
      }
    }
  }
  for (let at = 0; at < queue.length; at++) {
    const owner = queue[at];
    dependencies(owner, owner.data);
    if (Object.hasOwn(owner.data, "target")) {
      for (const [target, data] of Object.entries(table(owner.data.target, `${owner.path}: target`))) {
        dependencies(owner, table(data, `${owner.path}: target.${target}`));
      }
    }
  }
  for (const key of wanted) if (!sources.has(key)) fail(`missing declared local package source: ${key}`);
  // Commit both outputs only after the complete set has validated. Diagnostics use stderr.
  writeFileSync(process.argv[3], [...manifests.keys()].sort().join("\n") + "\n");
  process.stdout.write([...sources.values()].map((owner) => dirname(owner.path)).sort().join("\n") + "\n");
} catch (error) {
  console.error(`key.sh: ${error.message}`);
  process.exitCode = 1;
}
