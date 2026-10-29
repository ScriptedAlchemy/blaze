//! blaze's unit graph against cargo 1.99.0's `--unit-graph` for each run of each workspace of
//! `tests/units` (`expected.json`, written by `harness/dist/units_expected.rs` with the runs of the
//! fixture's `case.json`): the units, their features, profiles, modes and dependencies, as sets
//! keyed by what a unit is (its package, target, profile, platform, mode and features, and the
//! units it depends on and the names it uses them under), and the roots. Where cargo refuses a
//! run, blaze has to refuse it with cargo's message.
//!
//! The host's triple and cfg are those cargo's graphs were made on, `expected.json` carrying
//! them: nothing here runs rustc or cargo.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::str::FromStr;

use blaze_metadata::units::{Command, TargetFilter, UnitsAsked, unit_graph};
use blaze_metadata::{Asked, Build, TargetPlatform};
use blaze_resolve::Cfg;
use serde_json::Value;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "expected.json" || name == "case.json" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(&name));
        } else {
            std::fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

/// What a run of `cargo <args>` asks, as `blaze-spec` will read it from the arguments.
#[derive(Default)]
struct Run {
    command: Option<Command>,
    packages: Vec<String>,
    workspace: bool,
    excludes: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    profile: Option<String>,
    targets: TargetFilter,
    config: Vec<String>,
}

fn parse(args: &[String]) -> Run {
    let mut run = Run::default();
    let mut args = args.iter();
    run.command = Some(match args.next().unwrap().as_str() {
        "build" => Command::Build,
        "check" => Command::Check,
        "test" => Command::Test,
        other => panic!("`cargo {other}` is not a command of the unit graph"),
    });
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap().clone();
        let t = &mut run.targets;
        match arg.as_str() {
            "-p" | "--package" => run.packages.push(value()),
            "--workspace" | "--all" => run.workspace = true,
            "--exclude" => run.excludes.push(value()),
            "--features" | "-F" => run.features.push(value()),
            "--all-features" => run.all_features = true,
            "--no-default-features" => run.no_default_features = true,
            "--release" => run.profile = Some("release".to_owned()),
            "--profile" => run.profile = Some(value()),
            "--config" => run.config.push(value()),
            "--no-run" => {}
            "--lib" => {
                t.explicit = true;
                t.lib = true;
            }
            "--all-targets" => {
                t.explicit = true;
                t.all_targets = true;
            }
            "--bin" => push(&mut t.bins, Some(value()), &mut t.explicit),
            "--bins" => push(&mut t.bins, None, &mut t.explicit),
            "--example" => push(&mut t.examples, Some(value()), &mut t.explicit),
            "--examples" => push(&mut t.examples, None, &mut t.explicit),
            "--test" => push(&mut t.tests, Some(value()), &mut t.explicit),
            "--tests" => push(&mut t.tests, None, &mut t.explicit),
            "--bench" => push(&mut t.benches, Some(value()), &mut t.explicit),
            "--benches" => push(&mut t.benches, None, &mut t.explicit),
            other => panic!("the test does not read `{other}`"),
        }
    }
    run
}

/// `--bin NAME` adds a name, `--bins` makes it every one.
fn push(slot: &mut Option<Vec<String>>, name: Option<String>, explicit: &mut bool) {
    *explicit = true;
    match (name, slot.as_mut()) {
        (Some(name), Some(names)) if !names.is_empty() => names.push(name),
        (Some(name), None) => *slot = Some(vec![name]),
        (Some(_), Some(_)) => {}
        (None, _) => *slot = Some(Vec::new()),
    }
}

/// A key for a value that does not depend on the order of an object's keys.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map.iter().collect();
            out.push('{');
            for (key, value) in sorted {
                out.push_str(key);
                out.push(':');
                canonical(value, out);
                out.push(',');
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for item in items {
                canonical(item, out);
                out.push(',');
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A graph keyed by identity: each unit's hash over what it is and what it depends on, with a
/// label to show it by, and the roots' hashes. A unit cargo makes for doc tests is left out:
/// blaze runs doc tests apart from the build.
struct Keyed {
    units: Vec<(u64, String)>,
    roots: Vec<u64>,
}

fn keyed(graph: &Value, drop_doctests: bool) -> Keyed {
    let units = graph["units"].as_array().unwrap();
    let mut memo: Vec<Option<u64>> = vec![None; units.len()];
    fn hash_of(at: usize, units: &[Value], memo: &mut Vec<Option<u64>>) -> u64 {
        if let Some(done) = memo[at] {
            return done;
        }
        let unit = &units[at];
        let mut deps: Vec<String> = Vec::new();
        for dep in unit["dependencies"].as_array().unwrap() {
            let to = hash_of(dep["index"].as_u64().unwrap() as usize, units, memo);
            let mut text = String::new();
            let mut edge = dep.clone();
            edge["index"] = Value::Null;
            canonical(&edge, &mut text);
            deps.push(format!("{text}{to}"));
        }
        deps.sort();
        let mut text = String::new();
        let mut identity = unit.clone();
        identity["dependencies"] = Value::Null;
        canonical(&identity, &mut text);
        let mut hasher = DefaultHasher::new();
        (text, deps).hash(&mut hasher);
        let hash = hasher.finish();
        memo[at] = Some(hash);
        hash
    }
    let mut out = Keyed { units: Vec::new(), roots: Vec::new() };
    let dropped = |at: usize| drop_doctests && units[at]["mode"] == "doctest";
    for at in 0..units.len() {
        let hash = hash_of(at, units, &mut memo);
        if dropped(at) {
            continue;
        }
        let unit = &units[at];
        let label = format!(
            "{} {:?} {} {} {} features {}",
            unit["pkg_id"].as_str().unwrap().rsplit('#').next().unwrap(),
            unit["target"]["kind"],
            unit["target"]["name"],
            unit["mode"],
            unit["profile"]["name"],
            unit["features"],
        );
        out.units.push((hash, label));
    }
    for root in graph["roots"].as_array().unwrap() {
        let at = root.as_u64().unwrap() as usize;
        if !dropped(at) {
            out.roots.push(hash_of(at, units, &mut memo));
        }
    }
    out.units.sort();
    out.roots.sort();
    out
}

/// The differences between the two graphs, none when they are the same.
fn differences(want: &Value, got: &Value) -> Vec<String> {
    let want = normalized(want, "\0");
    let (want, got) = (keyed(&want, true), keyed(got, false));
    let mut out = Vec::new();
    let mut remaining = got.units.clone();
    for unit in &want.units {
        match remaining.iter().position(|it| it.0 == unit.0) {
            Some(at) => {
                remaining.remove(at);
            }
            None => out.push(format!("cargo has {}", unit.1)),
        }
    }
    for unit in remaining {
        out.push(format!("blaze has {}", unit.1));
    }
    if want.roots != got.roots {
        out.push(format!("roots differ: cargo {} blaze {}", want.roots.len(), got.roots.len()));
    }
    out
}

/// The `$ROOT`-relative text of blaze's graph, as cargo's was written.
fn normalized(graph: &Value, root: &str) -> Value {
    // Cargo omits a package's name when it is the URL's final path segment. The oracle's
    // throwaway directory is called `root`, whereas this test copies it into a directory
    // named for the fixture. Expand the compact id before replacing its absolute path.
    let mut graph = graph.clone();
    for unit in graph["units"].as_array_mut().unwrap() {
        let id = unit["pkg_id"].as_str().unwrap();
        if let Some((source, version)) = id.rsplit_once('#')
            && !version.contains('@')
            && version.starts_with(|c: char| c.is_ascii_digit())
        {
            let name = source.rsplit('/').next().unwrap();
            unit["pkg_id"] = Value::String(format!("{source}#{name}@{version}"));
        }
    }
    serde_json::from_str(&graph.to_string().replace(root, "$ROOT")).unwrap()
}

struct Outcome {
    runs: usize,
    equal: usize,
    refused: usize,
    failures: Vec<String>,
}

fn check(fixture: &Path, scratch: &Path) -> Outcome {
    let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
    let root = scratch.join(&name);
    copy_dir(fixture, &root);
    let root = root.canonicalize().unwrap();
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    let home = home.canonicalize().unwrap();
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.join("expected.json")).unwrap())
            .unwrap();
    let host = expected["host"].as_str().unwrap();
    let cfg: Vec<Cfg> = expected["cfg"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| Cfg::from_str(it.as_str().unwrap()).unwrap())
        .collect();
    let root_text = root.display().to_string();
    let mut out = Outcome { runs: 0, equal: 0, refused: 0, failures: Vec::new() };
    for run in expected["runs"].as_array().unwrap() {
        out.runs += 1;
        let args: Vec<String> = run["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|it| it.as_str().unwrap().to_owned())
            .collect();
        let what = format!("{name} `{}`", args.join(" "));
        let parsed = parse(&args);
        let mut env = vec![("CARGO_HOME".to_owned(), home.display().to_string())];
        for (key, value) in run["env"].as_object().into_iter().flatten() {
            env.push((key.clone(), value.as_str().unwrap().replace("$ROOT", &root_text)));
        }
        let asked = Asked {
            locked: false,
            cargo_home: None,
            cwd: &root,
            env,
            manifest_path: None,
            config: &parsed.config,
            features: &parsed.features,
            all_features: parsed.all_features,
            no_default_features: parsed.no_default_features,
            build: Some(Build {
                packages: &parsed.packages,
                workspace: parsed.workspace,
                excludes: &parsed.excludes,
                dev_units: false,
            }),
            platform: Some(TargetPlatform { triple: host, cfg: &cfg }),
            offline: true,
            store: None,
        };
        let units = UnitsAsked {
            asked: &asked,
            command: parsed.command.unwrap(),
            targets: &parsed.targets,
            profile: parsed.profile.as_deref(),
        };
        let result = unit_graph(&units);
        match (run["code"].as_u64().unwrap(), result) {
            (0, Ok(graph)) => {
                let got = normalized(&graph.to_cargo_json(), &root_text);
                let diffs = differences(&run["graph"], &got);
                if diffs.is_empty() {
                    out.equal += 1;
                } else {
                    out.failures.push(format!("{what}:\n  {}", diffs.join("\n  ")));
                }
            }
            (0, Err(err)) => out.failures.push(format!("{what}: blaze failed: {err:#}")),
            (_, Ok(_)) => out.failures.push(format!("{what}: cargo failed, blaze did not")),
            (_, Err(err)) => {
                let got = format!("error: {err:#}").replace(&root_text, "$ROOT");
                let want = run["error"].as_str().unwrap();
                if got.lines().next() == want.lines().next() {
                    out.refused += 1;
                } else {
                    out.failures.push(format!("{what}: error\n  {got}\ncargo's\n  {want}"));
                }
            }
        }
    }
    out
}

#[test]
fn unit_graphs_match_cargo() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/units");
    let scratch = std::env::temp_dir().join(format!("blaze-metadata-units-{}", std::process::id()));
    let mut names: Vec<_> = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.file_name())
        .collect();
    names.sort();
    let (mut runs, mut equal, mut refused) = (0, 0, 0);
    let mut failures = Vec::new();
    for name in &names {
        let outcome = check(&fixtures.join(name), &scratch.join(name));
        println!(
            "{}: {} runs, {} equal, {} refused as cargo refused them, {} different",
            name.to_string_lossy(),
            outcome.runs,
            outcome.equal,
            outcome.refused,
            outcome.failures.len()
        );
        runs += outcome.runs;
        equal += outcome.equal;
        refused += outcome.refused;
        failures.extend(outcome.failures);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "{} fixtures, {runs} runs: {equal} equal, {refused} refused, {} different",
        names.len(),
        failures.len()
    );
    assert!(runs >= 150, "only {runs} runs in {}", fixtures.display());
    assert!(failures.is_empty(), "{} differences:\n{}", failures.len(), failures.join("\n\n"));
}
