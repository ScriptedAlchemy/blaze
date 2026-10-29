//! Execute an authoritative source/producer graph without resolving a Cargo workspace.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};
use blaze_build_script::{Output, output::LinkArgTarget, unit};

use crate::cargo::{
    own_script::{self, Planned},
    producer_image,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Rust,
    BuildScript,
    ProcMacro,
}

pub(crate) struct Unit {
    pub(crate) source: Planned,
    pub(crate) kind: Kind,
    /// Participates in a host producer closure, including an identical shared unit.
    pub(crate) host: bool,
    pub(crate) target: unit::Target,
    /// The run whose output configures this unit, not the script compilation itself.
    pub(crate) run: Option<usize>,
    pub(crate) profile: Option<serde_json::Value>,
}

pub(crate) struct Run {
    pub(crate) compiler: usize,
    pub(crate) unit: unit::Unit,
    /// Direct dependency names and run indices, for DEP_<links> metadata.
    pub(crate) dependencies: Vec<(String, usize)>,
}

pub(crate) struct Graph {
    pub(crate) units: Vec<Unit>,
    pub(crate) runs: Vec<Run>,
}

pub(crate) struct Prepared {
    pub(crate) units: Vec<Planned>,
    pub(crate) native: BTreeMap<usize, NativeUnit>,
    pub(crate) outputs: BTreeMap<usize, (PathBuf, Output)>,
    pub(crate) inputs: BTreeSet<PathBuf>,
    pub(crate) runtime_inputs: BTreeSet<PathBuf>,
    pub(crate) generated_inputs: BTreeSet<PathBuf>,
    pub(crate) native_records: BTreeSet<PathBuf>,
    pub(crate) opaque_native: bool,
    _lease: File,
    _lock: File,
}

impl Prepared {
    fn observe(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        runtime: &BTreeSet<PathBuf>,
        generation: &Path,
    ) {
        for path in paths {
            if runtime.contains(&path) {
                self.runtime_inputs.insert(path);
            } else if path.starts_with(generation) {
                self.generated_inputs.insert(path);
            } else {
                self.inputs.insert(path);
            }
        }
    }
}

pub(crate) use blaze_wire::native::Unit as NativeUnit;

impl Graph {
    fn order(&self) -> Result<Vec<usize>> {
        let n = self.units.len();
        let mut edges = vec![Vec::new(); n + self.runs.len()];
        for (at, unit) in self.units.iter().enumerate() {
            ensure!(
                unit.source.proc_macro.is_none(),
                "captured graphs supply source, not macro artifacts"
            );
            ensure!(unit.source.root.is_absolute(), "captured source root is not absolute");
            for (_, dep) in &unit.source.deps {
                ensure!(*dep < n, "captured source dependency is outside its graph");
                ensure!(
                    self.units[*dep].kind != Kind::BuildScript,
                    "a build-script compilation is not a Rust library dependency"
                );
                edges[at].push(*dep);
            }
            if let Some(run) = unit.run {
                ensure!(run < self.runs.len(), "captured run is outside its graph");
                edges[at].push(n + run);
            }
        }
        fn host_closure(at: usize, units: &[Unit], seen: &mut BTreeSet<usize>) -> Result<()> {
            if seen.insert(at) {
                ensure!(units[at].host, "a captured host producer reaches target unit {at}");
                for (_, dep) in &units[at].source.deps {
                    host_closure(*dep, units, seen)?;
                }
            }
            Ok(())
        }
        let mut host_units = BTreeSet::new();
        for (at, unit) in self.units.iter().enumerate() {
            if unit.kind != Kind::Rust {
                host_closure(at, &self.units, &mut host_units)?;
            }
        }
        for (at, run) in self.runs.iter().enumerate() {
            ensure!(
                run.compiler < n && self.units[run.compiler].kind == Kind::BuildScript,
                "captured run has no build-script compilation"
            );
            edges[n + at].push(run.compiler);
            for (_, dep) in &run.dependencies {
                ensure!(*dep < self.runs.len(), "captured script dependency is outside its graph");
                edges[n + at].push(n + dep);
            }
        }
        fn visit(
            at: usize,
            edges: &[Vec<usize>],
            states: &mut [u8],
            order: &mut Vec<usize>,
        ) -> Result<()> {
            ensure!(states[at] != 1, "cycle in captured producer graph at {at}");
            if states[at] == 2 {
                return Ok(());
            }
            states[at] = 1;
            for &dep in &edges[at] {
                visit(dep, edges, states, order)?;
            }
            states[at] = 2;
            order.push(at);
            Ok(())
        }
        let mut states = vec![0; edges.len()];
        let mut order = Vec::new();
        for at in 0..edges.len() {
            visit(at, &edges, &mut states, &mut order)?;
        }
        Ok(order)
    }

    fn closure_indices(&self, root: usize, prepared: &[Planned]) -> Result<Vec<usize>> {
        fn visit(
            graph: &Graph,
            root: usize,
            at: usize,
            prepared: &[Planned],
            order: &mut Vec<usize>,
        ) -> Result<()> {
            if order.contains(&at) {
                return Ok(());
            }
            ensure!(graph.units[at].host, "a captured host producer reaches target unit {at}");
            order.push(at);
            if at != root && graph.units[at].kind == Kind::ProcMacro {
                ensure!(
                    prepared[at].proc_macro.is_some(),
                    "captured dependency macro was not built"
                );
                return Ok(());
            }
            for (_, dep) in &prepared[at].deps {
                visit(graph, root, *dep, prepared, order)?;
            }
            Ok(())
        }
        let mut order = Vec::new();
        visit(self, root, root, prepared, &mut order)?;
        Ok(order)
    }

    fn native_closure(
        &self,
        root: usize,
        prepared: &Prepared,
    ) -> Result<BTreeMap<u32, NativeUnit>> {
        self.closure_indices(root, &prepared.units)?
            .into_iter()
            .enumerate()
            .map(|(id, at)| Ok((u32::try_from(id)?, prepared.native[&at].clone())))
            .collect()
    }

    fn closure(&self, root: usize, prepared: &[Planned]) -> Result<Vec<Planned>> {
        let order = self.closure_indices(root, prepared)?;
        let indexes: BTreeMap<_, _> = order.iter().enumerate().map(|(i, &at)| (at, i)).collect();
        Ok(order
            .into_iter()
            .map(|at| {
                let mut source = prepared[at].clone();
                if at != root && self.units[at].kind == Kind::ProcMacro {
                    source.deps.clear();
                } else {
                    source.deps =
                        source.deps.into_iter().map(|(name, dep)| (name, indexes[&dep])).collect();
                }
                source
            })
            .collect())
    }
}

fn configure(
    source: &mut Planned,
    native: &mut NativeUnit,
    target: &unit::Target,
    run: &unit::Unit,
    output: &Output,
) {
    source.cfg.extend(output.cfgs.clone());
    source.env.extend(output.env.iter().cloned());
    native.env.extend(output.env.iter().map(|(key, value)| (key.into(), value.into())));
    native.env.insert("OUT_DIR".into(), run.out_dir.as_os_str().to_owned());
    if let Some(out) = run.out_dir.to_str() {
        source.env.insert("OUT_DIR".into(), out.into());
    } else {
        source.env.remove("OUT_DIR");
    }
    fn flags(source: &mut Planned, native: &mut NativeUnit, args: Vec<OsString>) {
        if let Some(text) = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>() {
            source.codegen_flags.extend(text.into_iter().map(str::to_owned));
        }
        native.flags.extend(args);
    }
    for path in &output.library_paths {
        flags(source, native, vec!["-L".into(), path.path().as_os_str().to_owned()]);
    }
    if target.is_lib() || !run.package.targets.iter().any(unit::Target::is_lib) {
        for library in &output.library_links {
            flags(source, native, vec!["-l".into(), library.into()]);
        }
    }
    for (kind, arg) in &output.linker_args {
        let applies = match kind {
            LinkArgTarget::All => {
                target.is_cdylib()
                    || target.is_bin()
                    || target.is_test()
                    || target.is_bench()
                    || target.is_exe_example()
            }
            LinkArgTarget::Cdylib => target.is_cdylib(),
            LinkArgTarget::Bin => target.is_bin(),
            LinkArgTarget::SingleBin(name) => target.is_bin() && target.name == *name,
            LinkArgTarget::Test => target.is_test(),
            LinkArgTarget::Bench => target.is_bench(),
            LinkArgTarget::Example => target.is_exe_example(),
        };
        if applies {
            flags(source, native, vec![format!("-Clink-arg={arg}").into()]);
        }
    }
}

fn read_inputs(path: &Path) -> Result<BTreeSet<PathBuf>> {
    let read = crate::dep_info::DepInfo::parse(&std::fs::read_to_string(path)?);
    let mut inputs = BTreeSet::new();
    for file in read.files {
        let file = PathBuf::from(file);
        inputs.insert(if file.is_absolute() {
            file
        } else {
            path.parent().context("input directory")?.join(file)
        });
    }
    Ok(inputs)
}

/// The caller retains the returned value through compilation and artifact admission.
pub(crate) fn run(
    graph: Graph,
    sysroot: &Path,
    work: &Path,
    environment: crate::process::Native,
) -> Result<Prepared> {
    let order = graph.order()?;
    let runtime = if graph.units.iter().any(|unit| unit.kind != Kind::Rust) {
        producer_image::runtime_paths(sysroot)?
    } else {
        BTreeSet::new()
    };
    crate::socket::private_dir(work)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(work.join(".execution"))?;
    lock.lock()?;
    let generations = work.join("generations");
    std::fs::create_dir_all(&generations)?;
    let generation = (0u64..)
        .map(|n| generations.join(format!("run-{n}")))
        .find(|path| !path.exists())
        .context("captured producer generation")?;
    let lease = load_cargo::proc_macro_producer_lifetime::lease(&generation)?;
    load_cargo::proc_macro_producer_lifetime::collect(&generations, &generation)?;
    let probe = generation.join("producer-rustc");
    crate::answers::producer_launcher(&probe)?;
    let failure = generation.join("unsupported-probe");
    let mut env: BTreeMap<OsString, OsString> = environment.vars().collect();
    env.insert("RUSTC".into(), probe.as_os_str().to_owned());
    env.insert("RUSTDOC".into(), probe.as_os_str().to_owned());
    env.insert("BLAZE_PRODUCER_PROBE_FAILURE".into(), failure.as_os_str().to_owned());
    env.remove(std::ffi::OsStr::new("RUSTC_WRAPPER"));
    env.remove(std::ffi::OsStr::new("RUSTC_WORKSPACE_WRAPPER"));
    let env: Vec<_> = env.into_iter().collect();
    let mut prepared = Prepared {
        units: graph.units.iter().map(|unit| unit.source.clone()).collect(),
        native: graph
            .units
            .iter()
            .enumerate()
            .map(|(at, unit)| {
                let mut native_env: BTreeMap<_, _> = environment.vars().collect();
                native_env
                    .extend(unit.source.env.iter().map(|(key, value)| (key.into(), value.into())));
                (
                    at,
                    NativeUnit {
                        env: native_env,
                        flags: unit.source.codegen_flags.iter().map(OsString::from).collect(),
                    },
                )
            })
            .collect(),
        outputs: BTreeMap::new(),
        inputs: BTreeSet::new(),
        runtime_inputs: BTreeSet::new(),
        generated_inputs: BTreeSet::new(),
        native_records: BTreeSet::new(),
        opaque_native: false,
        _lease: lease,
        _lock: lock,
    };
    let mut runs: BTreeMap<usize, unit::Unit> = BTreeMap::new();
    for action in order {
        if action < graph.units.len() {
            let node = &graph.units[action];
            if let Some(at) = node.run {
                let (_, output) = &prepared.outputs[&at];
                configure(
                    &mut prepared.units[action],
                    prepared.native.get_mut(&action).expect("captured native unit"),
                    &node.target,
                    &runs[&at],
                    output,
                );
            }
            if node.kind == Kind::ProcMacro {
                let crates = graph.closure(action, &prepared.units)?;
                let before = producer_image::snapshot(sysroot, &crates)?;
                prepared.observe(before.paths().cloned(), &runtime, &generation);
                let native = graph.native_closure(action, &prepared)?;
                let image = producer_image::link(
                    sysroot,
                    crates,
                    &generation.join(format!("macro-{action}")),
                    &env,
                    own_script::SourceMode::CapturedNative {
                        profile: node.profile.as_ref(),
                        units: &native,
                    },
                )?;
                ensure!(
                    producer_image::snapshot(sysroot, &graph.closure(action, &prepared.units)?)?
                        == before,
                    "captured macro source changed during compilation"
                );
                let dir = image.parent().context("macro image directory")?;
                prepared.observe(read_inputs(&dir.join("producer.inputs"))?, &runtime, &generation);
                let native = dir.join("producer.native.json");
                prepared.opaque_native |= !crate::native_inputs::image_still(&native);
                if native.is_file() {
                    prepared.native_records.insert(native);
                }
                prepared.units[action].proc_macro = Some(image);
                prepared.units[action].deps.clear();
            }
            continue;
        }
        let at = action - graph.units.len();
        let run = &graph.runs[at];
        let crates = graph.closure(run.compiler, &prepared.units)?;
        let before = producer_image::snapshot(sysroot, &crates)?;
        prepared.observe(before.paths().cloned(), &runtime, &generation);
        let dir = generation.join(format!("script-{at}"));
        let exe = dir.join(format!("build-script-build{}", std::env::consts::EXE_SUFFIX));
        let inputs = dir.join("producer.inputs");
        let native = graph.native_closure(run.compiler, &prepared)?;
        let described = own_script::with_mode(
            own_script::planned_description(sysroot, crates.clone()),
            own_script::SourceMode::CapturedNative {
                profile: graph.units[run.compiler].profile.as_ref(),
                units: &native,
            },
        )
        .map_err(anyhow::Error::msg)?;
        own_script::link_planned(&described, &dir, &exe, &inputs, &env)
            .map_err(anyhow::Error::msg)?;
        ensure!(
            producer_image::snapshot(sysroot, &crates)? == before,
            "captured script source changed during compilation"
        );
        prepared.observe(read_inputs(&inputs)?, &runtime, &generation);
        let native = inputs.with_extension("native.json");
        prepared.opaque_native |= !crate::native_inputs::image_still(&native);
        if native.is_file() {
            prepared.native_records.insert(native);
        }
        let mut unit = run.unit.clone();
        unit.script = exe;
        unit.out_dir = dir.join("out");
        unit.tools.rustc = probe.clone();
        unit.tools.rustdoc = probe.clone();
        unit.tools.rustc_wrapper = None;
        unit.tools.rustc_workspace_wrapper = None;
        unit.dependencies = run
            .dependencies
            .iter()
            .map(|(name, dep)| unit::Dependency {
                name: name.clone(),
                links: graph.runs[*dep].unit.package.links.clone(),
                metadata: prepared.outputs[dep].1.metadata.clone(),
            })
            .collect();
        let child_env = blaze_build_script::env(&unit, &env).map_err(|it| anyhow::anyhow!(it.0))?;
        let ran = blaze_build_script::run::run(
            &unit,
            blaze_build_script::run::Invocation::of(&unit, child_env).command(),
        )
        .map_err(|it| anyhow::anyhow!(it.0))?;
        ensure!(
            !failure.exists(),
            "captured producer compiler probe failed: {}",
            std::fs::read_to_string(&failure).unwrap_or_default()
        );
        let output = ran
            .finish(&unit, &blaze_build_script::output::Context::of(&unit, &env))
            .map_err(|it| anyhow::anyhow!(it.error.0))?;
        for input in &output.rerun_if_changed {
            let path = if input.is_absolute() {
                input.clone()
            } else {
                unit.package.manifest_dir.join(input)
            };
            prepared.observe([path], &runtime, &generation);
        }
        prepared.outputs.insert(at, (unit.out_dir.clone(), output));
        runs.insert(at, unit);
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, kind: Kind, host: bool, deps: &[(&str, usize)]) -> Unit {
        Unit {
            source: Planned {
                name: name.into(),
                root: std::env::temp_dir().join(format!("{name}.rs")),
                edition: "2021".into(),
                cfg: Vec::new(),
                codegen_flags: vec!["-Copt-level=3".into()],
                env: BTreeMap::new(),
                deps: deps.iter().map(|(name, at)| ((*name).into(), *at)).collect(),
                proc_macro: None,
            },
            kind,
            host,
            target: unit::Target {
                name: name.into(),
                kind: vec!["lib".into()],
                crate_types: vec!["rlib".into()],
            },
            run: None,
            profile: None,
        }
    }

    fn graph() -> Graph {
        let mut graph = Graph {
            units: vec![
                node("app", Kind::Rust, false, &[("derive", 1)]),
                node("derive", Kind::ProcMacro, true, &[("shared", 2)]),
                node("shared", Kind::Rust, true, &[]),
                node("build_script_build", Kind::BuildScript, true, &[("shared", 2)]),
            ],
            runs: vec![Run { compiler: 3, unit: unit::Unit::default(), dependencies: Vec::new() }],
        };
        graph.units[1].run = Some(0);
        graph
    }

    #[test]
    fn scripts_precede_the_macro_and_target_consumers() {
        assert_eq!(graph().order().unwrap(), [2, 3, 4, 1, 0]);
    }

    #[test]
    fn invalid_or_cyclic_graphs_fail_before_execution() {
        let mut cyclic = graph();
        cyclic.units[3].run = Some(0);
        assert!(cyclic.order().unwrap_err().to_string().contains("cycle"));
        let mut invalid = graph();
        invalid.units[0].source.deps.push(("missing".into(), 99));
        assert!(invalid.order().unwrap_err().to_string().contains("outside"));
        let mut target_leak = graph();
        target_leak.units[2].host = false;
        assert!(target_leak.order().unwrap_err().to_string().contains("target unit"));
    }

    #[test]
    fn nested_macro_closures_use_the_admitted_image_as_a_leaf() {
        let mut graph = graph();
        graph.units.push(node("nested", Kind::ProcMacro, true, &[("shared", 2)]));
        graph.units[1].source.deps.push(("nested_alias".into(), 4));
        let mut prepared: Vec<_> = graph.units.iter().map(|unit| unit.source.clone()).collect();
        prepared[4].proc_macro = Some(std::env::temp_dir().join("nested-image"));
        let closure = graph.closure(1, &prepared).unwrap();
        assert_eq!(closure[0].deps, [("shared".into(), 1), ("nested_alias".into(), 2)]);
        assert!(closure[2].proc_macro.is_some());
        assert!(closure[2].deps.is_empty());
    }

    #[test]
    fn native_directives_keep_library_and_program_target_rules() {
        let library = unit::Target {
            name: "parts".into(),
            kind: vec!["lib".into()],
            crate_types: vec!["rlib".into()],
        };
        let binary = unit::Target {
            name: "app".into(),
            kind: vec!["bin".into()],
            crate_types: vec!["bin".into()],
        };
        let run = unit::Unit {
            package: unit::Package {
                targets: vec![library.clone(), binary.clone()],
                ..Default::default()
            },
            out_dir: PathBuf::from("out"),
            ..Default::default()
        };
        let output = Output {
            cfgs: vec!["generated".into()],
            env: vec![("GENERATED".into(), "value".into())],
            library_links: vec!["static=generated".into()],
            linker_args: vec![
                (LinkArgTarget::All, "all-programs".into()),
                (LinkArgTarget::SingleBin("app".into()), "named-program".into()),
            ],
            ..Default::default()
        };
        let mut lib = node("parts", Kind::Rust, false, &[]).source;
        configure(&mut lib, &mut NativeUnit::default(), &library, &run, &output);
        assert!(lib.codegen_flags.contains(&"static=generated".into()));
        assert!(!lib.codegen_flags.iter().any(|flag| flag.starts_with("-Clink-arg=")));
        let mut bin = node("app", Kind::Rust, false, &[]).source;
        configure(&mut bin, &mut NativeUnit::default(), &binary, &run, &output);
        assert!(!bin.codegen_flags.contains(&"static=generated".into()));
        assert!(bin.codegen_flags.contains(&"-Clink-arg=named-program".into()));
        assert_eq!(bin.env["GENERATED"], "value");
        assert_eq!(bin.cfg, ["generated"]);
    }

    #[cfg(unix)]
    #[test]
    fn generated_native_paths_are_retained_without_a_text_conversion() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = PathBuf::from(OsString::from_vec(b"/generated-\xff".to_vec()));
        let target = unit::Target {
            name: "parts".into(),
            kind: vec!["lib".into()],
            crate_types: vec!["rlib".into()],
        };
        let run = unit::Unit { out_dir: raw.clone(), ..Default::default() };
        let output = Output {
            library_paths: vec![blaze_build_script::output::LibraryPath::External(raw.clone())],
            ..Default::default()
        };
        let mut source = node("parts", Kind::Rust, true, &[]).source;
        let mut native = NativeUnit::default();
        configure(&mut source, &mut native, &target, &run, &output);
        assert_eq!(native.env[std::ffi::OsStr::new("OUT_DIR")], raw.as_os_str());
        assert_eq!(native.flags, [OsString::from("-L"), raw.into_os_string()]);
        assert!(!source.codegen_flags.contains(&"-L".into()));
    }
}
