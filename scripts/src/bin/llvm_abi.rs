//! Generate the emitter's owned C FFI from the pinned LLVM headers.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, File},
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    TempDir, Value,
    child::{self, Ended},
    fail,
    input::{self, Fields, Input, InputError},
};

const SCRIPT: &str = "llvm_abi";
const DEADLINE: Duration = Duration::from_secs(60);
const AST_BYTES: u64 = 64 * 1024 * 1024;
const TEXT_BYTES: u64 = 1024 * 1024;
const TYPE_DEPTH: u8 = 32;

struct Args {
    prefix: PathBuf,
    clang: String,
    check: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["prefix", "clang", "check"];

    fn parse(fields: &Fields<'_>) -> Result<Self, InputError> {
        let prefix = fields.str("prefix", None)?;
        let clang = fields.str("clang", Some("clang"))?;
        if prefix.is_empty() || clang.is_empty() {
            return Err(InputError("prefix and clang must be nonempty".into()));
        }
        Ok(Self { prefix: prefix.into(), clang, check: fields.bool("check", Some(false))? })
    }
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|why| format!("{}: {why}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|why| format!("{}: {why}", path.display()))?;
    if !u64::try_from(bytes.len()).is_ok_and(|len| len <= limit) {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(bytes)
}

fn json(path: &Path, limit: u64) -> Result<Value, String> {
    let bytes = read(path, limit)?;
    serde_json::from_slice(&bytes).map_err(|why| format!("{}: {why}", path.display()))
}

fn field<'a>(node: &'a Value, name: &str) -> Result<&'a str, String> {
    node[name].as_str().ok_or_else(|| format!("Clang AST node has no string field {name}"))
}

fn children(node: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    node["inner"].as_array().into_iter().flatten()
}

fn preorder(root: &Value) -> impl Iterator<Item = &Value> {
    let mut pending = vec![root];
    std::iter::from_fn(move || {
        let node = pending.pop()?;
        pending.extend(children(node).rev());
        Some(node)
    })
}

fn qual_type(node: &Value) -> Result<&str, String> {
    field(&node["type"], "qualType")
}

struct Types<'a> {
    nodes: BTreeMap<&'a str, &'a Value>,
    typedefs: BTreeMap<&'a str, &'a Value>,
    /// Declarations stay in dependency order so regeneration preserves the committed form.
    definitions: Vec<(&'a str, String)>,
    enum_checks: String,
}

impl<'a> Types<'a> {
    fn rust(&mut self, ty: &'a str, depth: u8) -> Result<String, String> {
        let ty = ty.trim();
        if depth == 0 {
            return Err(format!("C type {ty:?} exceeds the type depth limit of {TYPE_DEPTH}"));
        }
        if let Some(pointee) = ty.strip_suffix('*') {
            let pointee = pointee.trim();
            let (prefix, ty) = match pointee.strip_prefix("const ") {
                Some(ty) => ("*const", ty),
                None => ("*mut", pointee),
            };
            return Ok(format!("{prefix} {}", self.rust(ty, depth - 1)?));
        }
        let primitive = match ty {
            "void" => Some("core::ffi::c_void"),
            "char" => Some("core::ffi::c_char"),
            "int" => Some("core::ffi::c_int"),
            "unsigned int" => Some("core::ffi::c_uint"),
            "size_t" => Some("usize"),
            _ => None,
        };
        if let Some(primitive) = primitive {
            return Ok(primitive.into());
        }
        let name = ty.strip_prefix("struct ").unwrap_or(ty);
        if self.definitions.iter().any(|&(defined, _)| defined == name) {
            return Ok(name.into());
        }
        let definition = if name != ty {
            format!("#[repr(C)]\npub struct {name} {{ _private: [u8; 0] }}\n")
        } else if let Some(&typedef) = self.typedefs.get(name) {
            let written = qual_type(typedef)?;
            if written.starts_with("enum ") {
                self.enum_of(name, typedef)?
            } else {
                format!("pub type {name} = {};\n", self.rust(written, depth - 1)?)
            }
        } else {
            return Err(format!("unhandled C type {ty:?}"));
        };
        self.definitions.push((name, definition));
        Ok(name.into())
    }

    fn enum_of(&mut self, name: &str, typedef: &Value) -> Result<String, String> {
        let tag_id = preorder(typedef)
            .find_map(|node| node["ownedTagDecl"]["id"].as_str())
            .ok_or_else(|| format!("C enum {name} has no owned tag declaration"))?;
        let tag = self.nodes.get(tag_id).ok_or_else(|| format!("C enum {name} has no tag {tag_id}"))?;
        let mut written = format!("#[repr(C)]\n#[derive(Clone, Copy)]\npub enum {name} {{\n");
        let mut last = -1i32;
        let mut count = 0;
        for row in children(tag).filter(|node| node["kind"] == "EnumConstantDecl") {
            let variant = field(row, "name")?;
            last = if let Some(value) = preorder(row).find(|node| node["kind"] == "ConstantExpr") {
                field(value, "value")?
                    .parse()
                    .map_err(|why| format!("C enum {name} variant {variant} must fit an int: {why}"))?
            } else {
                last.checked_add(1).ok_or_else(|| format!("C enum {name} variant {variant} overflows int"))?
            };
            count += 1;
            let _ = writeln!(written, "    {variant} = {last},");
            let _ = writeln!(self.enum_checks, "static_assert({variant} == {last});");
        }
        if count == 0 {
            return Err(format!("C enum {name} has no variants"));
        }
        let _ = writeln!(self.enum_checks, "static_assert(sizeof({name}) == sizeof(int));");
        written.push_str("}\n");
        Ok(written)
    }
}

fn bindings(ast: &Value, selected: &str, version: &str, commit: &str) -> Result<(String, String), String> {
    if ast["kind"] != "TranslationUnitDecl" || !ast["inner"].is_array() {
        return Err("Clang AST has no translation unit declarations".into());
    }
    let declared = |kind: &str| {
        children(ast)
            .filter(move |node| node["kind"] == kind)
            .filter_map(|node| Some((node["name"].as_str()?, node)))
            .collect::<BTreeMap<_, _>>()
    };
    let functions = declared("FunctionDecl");
    let mut types = Types {
        nodes: preorder(ast)
            .filter_map(|node| Some((node["id"].as_str()?, node)))
            .collect(),
        typedefs: declared("TypedefDecl"),
        definitions: Vec::new(),
        enum_checks: String::new(),
    };
    let mut declarations = String::new();
    let mut checks = String::from(
        "// Generated by llvm_abi. Checked against the installed pinned headers.\n#include \"abi.h\"\n#include <type_traits>\n",
    );
    let mut selected = selected.split_whitespace().peekable();
    if selected.peek().is_none() {
        return Err("functions.txt has no selected C functions".into());
    }
    for name in selected {
        let function = functions.get(name).ok_or_else(|| format!("pinned C headers do not declare {name}"))?;
        if function["variadic"] == true {
            return Err(format!("{name} has a variadic C signature"));
        }
        let return_type = qual_type(function)?.split_once('(')
            .ok_or_else(|| format!("{name} has no C function signature"))?.0.trim();
        let mut c_parameters = Vec::new();
        let mut rust_parameters = Vec::new();
        for (index, param) in children(function).filter(|node| node["kind"] == "ParmVarDecl").enumerate() {
            let c = qual_type(param)?;
            c_parameters.push(c);
            let rust = types.rust(c, TYPE_DEPTH).map_err(|why| format!("{name} argument {index}: {why}"))?;
            rust_parameters.push(format!("arg{index}: {rust}"));
        }
        let result = if return_type == "void" {
            String::new()
        } else {
            let rust = types.rust(return_type, TYPE_DEPTH).map_err(|why| format!("{name} result: {why}"))?;
            format!(" -> {rust}")
        };
        let rust_parameters = rust_parameters.join(", ");
        let c_parameters = c_parameters.join(", ");
        let _ = writeln!(declarations, "    pub fn {name}({rust_parameters}){result};");
        let _ = writeln!(checks, "static_assert(std::is_same_v<decltype(&{name}), {return_type} (*)({c_parameters})>);");
    }
    let mut rust = format!(
        "//! Generated by llvm_abi from abi.h and functions.txt.\n//! LLVM {version}, commit {commit}.\n#![no_std]\n#![allow(non_camel_case_types, non_snake_case)]\n",
    );
    for (_, definition) in types.definitions {
        rust.push('\n');
        rust.push_str(&definition);
    }
    rust.push_str("\nunsafe extern \"C\" {\n");
    rust.push_str(&declarations);
    rust.push_str("}\n");
    checks.push_str(&types.enum_checks);
    Ok((rust, checks))
}

fn file_name(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

fn clang_ast(clang: String, prefix: &Path, header: &Path) -> Result<Value, String> {
    let scratch = TempDir::new("blaze-llvm-abi-").map_err(|why| format!("creating a Clang output directory: {why}"))?;
    let log = scratch.path().join("clang");
    let cwd = std::env::current_dir().map_err(|why| format!("reading the working directory: {why}"))?;
    let env: Vec<_> = ["PATH", "SDKROOT", "DEVELOPER_DIR"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect();
    let argv = [
        clang, "-x".into(), "c".into(), "-I".into(), file_name(&prefix.join("include"))?,
        "-Xclang".into(), "-ast-dump=json".into(), "-fsyntax-only".into(), file_name(header)?,
    ];
    match child::run_logged(&argv, &cwd, &env, &log, DEADLINE)? {
        Ended::Exited(run) if run.status == 0 => json(&log.with_extension("out"), AST_BYTES)
            .map_err(|why| format!("{} Clang AST for {}: {why}", argv[0], header.display())),
        Ended::Exited(run) => {
            let said = read(&log.with_extension("err"), TEXT_BYTES)?;
            Err(format!("{} exited {} parsing {}: {}", argv[0], run.status, header.display(), String::from_utf8_lossy(&said)))
        }
        Ended::TimedOut { after } => Err(format!("{} took longer than {after:?} parsing {}", argv[0], header.display())),
    }
}

fn run(args: Args) -> Result<(), String> {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = scripts.join("../crates/blaze-llvm-abi");
    let pin_path = scripts.join("../pins/llvm.json");
    let pin = json(&pin_path, TEXT_BYTES)?;
    let prefix = args.prefix.canonicalize().map_err(|why| format!("{}: {why}", args.prefix.display()))?;
    let marker_path = prefix.join("blaze-llvm.json");
    let marker = json(&marker_path, TEXT_BYTES)?;
    let pin_value = |name: &str| pin[name].as_str().ok_or_else(|| format!("{} has no string field {name}", pin_path.display()));
    let version = pin_value("version")?;
    let commit = pin_value("commit")?;
    for (name, expected) in [("version", version), ("commit", commit)] {
        if marker[name].as_str() != Some(expected) {
            return Err(format!("{}: {name} is not {}'s {expected}", marker_path.display(), pin_path.display()));
        }
    }
    let ast = clang_ast(args.clang, &prefix, &dir.join("abi.h"))?;
    let selected_path = dir.join("functions.txt");
    let selected = read(&selected_path, TEXT_BYTES)?;
    let selected = std::str::from_utf8(&selected).map_err(|why| format!("{}: {why}", selected_path.display()))?;
    let (rust, cpp) = bindings(&ast, selected, version, commit)?;
    for (name, text) in [("src/lib.rs", rust), ("abi-checks.cpp", cpp)] {
        let path = dir.join(name);
        if args.check {
            if read(&path, TEXT_BYTES)? != text.as_bytes() {
                return Err(format!("{}: regenerate from the pinned headers", path.display()));
            }
        } else {
            fs::write(&path, text).map_err(|why| format!("{}: {why}", path.display()))?;
        }
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        fail(SCRIPT, "expected one JSON input with prefix, optional clang and optional check");
    };
    if args.next().is_some() {
        fail(SCRIPT, "expected one JSON input");
    }
    if let Err(why) = run(input::parse(&input, SCRIPT)) {
        fail(SCRIPT, &why);
    }
}
