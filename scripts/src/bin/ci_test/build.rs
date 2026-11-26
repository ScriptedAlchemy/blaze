//! Partition Cargo's resolved test units, keeping the workspace package roots in every build.
use super::*;

fn command(args: &Args, tc: &str) -> Vec<String> {
    let mut argv: Vec<String> =
        ["cargo", &format!("+{tc}"), "test", "--locked", "--workspace", "--no-run"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    for package in &args.exclude {
        argv.extend(["--exclude".to_owned(), package.clone()]);
    }
    argv
}

fn logged(argv: &[String], root: &Path, log: &Path, seconds: u64) -> Result<Ended, String> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let env: Vec<(&str, String)> = env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    run_logged(argv, root, &env, log, Duration::from_secs(seconds))
}

pub(super) fn query(argv: &[String], root: &Path, log: &Path) -> String {
    match logged(argv, root, log, 30) {
        Ok(Ended::Exited(m)) if m.status == 0 => {
            read_lossy(&log.with_extension("out")).trim().to_owned()
        }
        result => fail(
            SCRIPT,
            &format!("{argv:?}: {result:?}; {}", read_lossy(&log.with_extension("err"))),
        ),
    }
}

fn write_json(path: &Path, value: &Value) {
    let bytes = serde_json::to_vec_pretty(value).expect("JSON value serializes");
    std::fs::write(path, bytes)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", path.display())));
}

/// Equal target names stay together: Cargo's --test selector matches across package roots.
fn targets(graph: &Value, partitions: usize) -> Result<BTreeMap<String, usize>, String> {
    let units =
        graph.get("units").and_then(Value::as_array).ok_or("Cargo unit graph has no units")?;
    let mut names = BTreeSet::new();
    for unit in units {
        if unit.get("mode").and_then(Value::as_str) != Some("test") {
            continue;
        }
        let target = &unit["target"];
        if target["kind"].as_array().is_some_and(|kinds| kinds.iter().any(|k| k == "test")) {
            let name = target["name"].as_str().ok_or("Cargo integration test has no name")?;
            names.insert(name.to_owned());
        }
    }
    if partitions > 1 && names.len() < partitions - 1 {
        return Err(format!(
            "{} integration target names cannot fill {} partitions",
            names.len(),
            partitions - 1
        ));
    }
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let partition = if partitions == 1 { 0 } else { 1 + index % (partitions - 1) };
            (name, partition)
        })
        .collect())
}

pub(super) fn run(args: &Args, tc: &str, root: &Path, evidence: &Path) -> Vec<(String, String)> {
    std::fs::create_dir_all(evidence)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", evidence.display())));
    let mut argv = command(args, tc);
    if args.partitions > 1 {
        let mut graph_argv = argv.clone();
        graph_argv.extend(["--unit-graph".to_owned(), "-Zunstable-options".to_owned()]);
        let graph_log = evidence.join("unit-graph");
        match logged(&graph_argv, root, &graph_log, 120) {
            Ok(Ended::Exited(m)) if m.status == 0 => {}
            result => fail(
                SCRIPT,
                &format!(
                    "Cargo unit graph failed: {result:?}; {}",
                    read_lossy(&graph_log.with_extension("err"))
                ),
            ),
        }
        let graph: Value = serde_json::from_str(&read_lossy(&graph_log.with_extension("out")))
            .unwrap_or_else(|e| fail(SCRIPT, &format!("Cargo unit graph JSON: {e}")));
        let assignment = targets(&graph, args.partitions).unwrap_or_else(|e| fail(SCRIPT, &e));
        write_json(
            &evidence.join("partitions.json"),
            &serde_json::json!({
                "schema": 1, "partition": args.partition, "partitions": args.partitions,
                "integration_targets": assignment,
                "base": "library and enabled binary tests, example builds, workspace doctests",
            }),
        );
        if args.partition == 0 {
            argv.push("--lib".to_owned());
            let mut binaries = BTreeSet::new();
            for unit in graph["units"].as_array().expect("validated units") {
                if unit["mode"] == "test" {
                    for kind in ["bin", "example"] {
                        if unit["target"]["kind"]
                            .as_array()
                            .is_some_and(|k| k.iter().any(|k| k == kind))
                        {
                            binaries.insert((
                                kind,
                                unit["target"]["name"].as_str().expect("Cargo target name"),
                            ));
                        }
                    }
                }
            }
            for (kind, name) in binaries {
                argv.extend([format!("--{kind}"), name.to_owned()]);
            }
            // cargo test's default examples are build units, not test harnesses. --examples
            // on cargo test would force test=false examples into harnesses and lose coverage
            // of their actual main functions. Keep the original build mode explicitly.
            let mut examples = command(args, tc);
            examples[2] = "build".to_owned();
            examples.retain(|arg| arg != "--no-run");
            examples.extend([
                "--examples".to_owned(),
                "--message-format=json-render-diagnostics".to_owned(),
            ]);
            write_json(&evidence.join("examples-command.json"), &serde_json::json!(examples));
            let example_log = evidence.join("examples");
            match logged(&examples, root, &example_log, args.build_timeout_s) {
                Ok(Ended::Exited(m)) if m.status == 0 => {}
                result => fail(
                    SCRIPT,
                    &format!(
                        "Cargo example build failed: {result:?}; {}",
                        read_lossy(&example_log.with_extension("err"))
                    ),
                ),
            }
        } else {
            for (name, partition) in &assignment {
                if *partition == args.partition {
                    argv.extend(["--test".to_owned(), name.clone()]);
                }
            }
        }
    }
    argv.push("--message-format=json-render-diagnostics".to_owned());
    write_json(&evidence.join("build-command.json"), &serde_json::json!(argv));
    // run_logged opens these files before spawning Cargo. An outer kill still leaves every
    // emitted artifact event on disk; the workflow's always-step reconstructs its inventory.
    let log = evidence.join("cargo-test");
    let ended = logged(&argv, root, &log, args.build_timeout_s);
    let code = match &ended {
        Ok(Ended::Exited(m)) => Some(m.status),
        _ => None,
    };
    let stdout = std::fs::read(log.with_extension("out"))
        .unwrap_or_else(|e| fail(SCRIPT, &format!("reading Cargo events: {e}")));
    let bins = parse_artifacts(&String::from_utf8_lossy(&stdout));
    preserve_build(evidence, &stdout, &bins, code == Some(0), code)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("preserving Cargo events: {e}")));
    write_json(
        &evidence.join("build-result.json"),
        &serde_json::json!({"exit_code":code,"complete":true}),
    );
    println!("{SCRIPT}: {} completed test executables; cargo exit {code:?}", bins.len());
    if code != Some(0) {
        eprint!("{}", read_lossy(&log.with_extension("err")));
        fail(SCRIPT, &format!("Cargo test build failed: {ended:?}"));
    }
    bins
}

/// Called by the workflow even after its outer deadline killed ci_test.
pub(super) fn recover(evidence: &Path) {
    let raw = evidence.join("cargo-test.out");
    let stdout = match std::fs::read(&raw) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => fail(SCRIPT, &format!("{}: {e}", raw.display())),
    };
    if evidence.join("build-result.json").exists() {
        return;
    }
    let bins = parse_artifacts(&String::from_utf8_lossy(&stdout));
    preserve_build(evidence, &stdout, &bins, false, None)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("recovering interrupted build inventory: {e}")));
    println!("{SCRIPT}: recovered {} linked test executables from interrupted build", bins.len());
}

pub(super) fn selftest() {
    let mut units = Vec::new();
    for name in ["a", "b", "b", "c", "d", "e", "f"] {
        units.push(serde_json::json!({"mode":"test", "target":{"kind":["test"],"name":name}}));
    }
    units.push(serde_json::json!({"mode":"build", "target":{"kind":["test"],"name":"inactive"}}));
    units.push(serde_json::json!({"mode":"test", "target":{"kind":["lib"],"name":"library"}}));
    let graph = serde_json::json!({"units":units});
    let assignment = targets(&graph, 4).expect("valid unit graph");
    assert_eq!(assignment.len(), 6);
    for partition in 1..4 {
        assert_eq!(assignment.values().filter(|p| **p == partition).count(), 2);
    }
    assert!(!assignment.contains_key("inactive"));
    assert!(!assignment.contains_key("library"));
    assert!(targets(&graph, 8).is_err());
    let tmp = make_tmp_dir();
    let raw = b"{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/w/Cargo.toml\",\"profile\":{\"test\":true},\"executable\":\"/t/test\"}\n{\"reason\":";
    std::fs::write(tmp.0.join("cargo-test.out"), raw).expect("interrupted stream writes");
    recover(&tmp.0);
    let inventory: Value =
        serde_json::from_str(&read_lossy(&tmp.0.join("executables.json"))).expect("inventory JSON");
    assert_eq!(inventory["build_success"], false);
    assert_eq!(inventory["build_exit_code"], Value::Null);
    assert_eq!(inventory["executables"].as_array().expect("executables").len(), 1);
    assert_eq!(std::fs::read(tmp.0.join("cargo-test.jsonl")).expect("retained raw"), raw);
}
