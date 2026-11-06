//! The graph as `cargo build --unit-graph -Z unstable-options` prints it, on a toolchain that
//! allows unstable features (`RUSTC_BOOTSTRAP=1`): the edges say `public`, `noprelude` and
//! `nounused`.

use serde_json::{Value, json};

use super::{Mode, TargetKind, Unit, UnitGraph, UnitTarget};

pub(super) fn of(graph: &UnitGraph) -> Value {
    // Cargo's comparison format omits documentation tests; preserve all actual
    // compile units, including those reached only from a doctest context.
    let mut ranks = vec![None; graph.units.len()];
    let mut units = Vec::new();
    for (at, source) in graph.units.iter().enumerate() {
        if source.mode != Mode::DocTest {
            ranks[at] = Some(units.len());
            units.push(unit(source));
        }
    }
    for unit in &mut units {
        for dep in unit["dependencies"].as_array_mut().unwrap() {
            let old = dep["index"].as_u64().unwrap() as usize;
            dep["index"] = json!(ranks[old].expect("compile units never depend on doctests"));
        }
    }
    let roots: Vec<_> = graph.roots.iter().filter_map(|&at| ranks[at]).collect();
    json!({ "version": 1, "units": units, "roots": roots })
}

fn unit(unit: &Unit) -> Value {
    let mode = match unit.mode {
        Mode::Build => "build",
        Mode::DocTest => unreachable!("doctest contexts are omitted from Cargo JSON"),
        Mode::Check => "check",
        Mode::Test => "test",
        Mode::RunCustomBuild => "run-custom-build",
    };
    let dependencies: Vec<Value> = unit
        .dependencies
        .iter()
        .map(|dep| {
            json!({
                "index": dep.index,
                "extern_crate_name": dep.extern_crate_name,
                "public": dep.public,
                "noprelude": dep.noprelude,
                "nounused": dep.nounused,
            })
        })
        .collect();
    json!({
        "pkg_id": unit.package.id,
        "target": target(&unit.target),
        "profile": serde_json::to_value(&unit.profile).expect("a profile is JSON"),
        // Nothing is built for another target than the host.
        "platform": Value::Null,
        "mode": mode,
        "features": unit.features,
        "dependencies": dependencies,
    })
}

/// The target as cargo prints it: `kind` is the crate types of a library and the target's own
/// kind otherwise.
fn target(target: &UnitTarget) -> Value {
    let kind: Vec<&str> = match target.kind {
        TargetKind::Lib | TargetKind::ProcMacro => {
            target.crate_types.iter().map(String::as_str).collect()
        }
        TargetKind::Bin => vec!["bin"],
        TargetKind::Test => vec!["test"],
        TargetKind::Example => vec!["example"],
        TargetKind::Bench => vec!["bench"],
        TargetKind::CustomBuild => vec!["custom-build"],
    };
    let mut out = json!({
        "kind": kind,
        "crate_types": target.crate_types,
        "name": target.name,
        "src_path": target.src_path,
        "edition": target.edition.to_string(),
    });
    if let Some(required) = &target.required_features {
        out["required-features"] = json!(required);
    }
    out["doc"] = json!(target.doc);
    out["doctest"] = json!(target.doctest);
    out["test"] = json!(target.tested);
    out
}
