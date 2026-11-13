#!/usr/bin/env python3
"""Measure the selected source-built Blaze and compare its stored regression metrics.

One strict JSON object, inline, a file path, or `-` for standard input:
    {"action":"run", "blaze":"/path/to/blaze", "work":"/fresh/bench",
     "out":"/results/bench.json", "projects":["ripgrep"], "runs":3}
    {"action":"compare", "baseline":"OLD.json", "result":"NEW.json", "threshold":25}
Run and Compare below define every field. No benchmark option is read from the environment.

Each pinned checkout is copied into work. Each profile has its own daemon, home, store and
target. The phases are cold, no-op, every configured edit and its reversal, and a second
checkout with the first one's stored units. Optional tests use each project's own verdict;
runtime workloads time the program built by Blaze. Every successful build must identify the
selected daemon, and an edit must splice the existing image. Programs must keep the output
and exit status of their first cold build. A failure is retained and exits 3.

Schema 2 contains only Blaze measurements: wall time, samples, memory, work, splice,
instructions, cycles, disk, tests and runtime. It contains no Cargo comparison or alternate
rustc setup. Historical result files stay in their original schema and keep their Cargo
labels. Compare uses their stored Blaze metrics, refuses missing phases or previously
measured Blaze fields, and holds daemon and splice behavior exactly. Timing differences of
50 ms or less do not count. Both profiles use the exact selected binary with --ignore-pin:
cranelift is dev, llvm is --release. The binary's path and SHA-256 are recorded and rechecked.
"""
import glob, hashlib, importlib, json, math, os, platform, re, shutil, statistics, subprocess, sys, time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Literal

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchschema, blaze_cli, counters, memory, storelib
from blaze_cli import HERE, ROOT, TOOLCHAIN, Side, fetch, how_built, program_output, test_results, test_verdict

LET = "\n    let _bench_edit = 1u32;"


def body(file, opening):
    """An edit that adds a statement after the line that opens a function."""
    return [(file, opening, opening + LET)]


def parameter(file, name, returns, callers):
    """An edit that gives the method `name` a parameter and every call in `callers` the argument."""
    old = f"fn {name}(&self) -> {returns} {{"
    return [(file, old, old.replace("(&self)", "(&self, _bench_edit: ())")), (callers, f".{name}()", f".{name}(())")]


def new_function(file, opening):
    """An edit that adds a function at the end of the file and calls it from the function
    that `opening` opens."""
    return [(file, opening, opening + "\n    let _bench_edit = bench_added(1u32);"),
            (file, None, "\n#[inline(never)]\nfn bench_added(x: u32) -> u32 {\n    x.wrapping_mul(31)\n}\n")]


def field(file, header, literals):
    """An edit that adds a field to the struct whose definition opens with `header`, and to
    every literal of it, which `literals` names as (pattern of files, the text that opens the
    literal). The field is `()`, so that no output and no layout the program shows changes."""
    return [(file, header, header + "\n    pub _bench_edit: (),")] + [
        (files, opening, opening + "\n            _bench_edit: (),") for files, opening in literals]


PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=dict(
            body=body("crates/core/main.rs", "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"),
            signature=parameter("crates/printer/src/standard.rs", "write_line_term", "io::Result<()>", "crates/printer/src/standard.rs"),
            leaf=body("crates/matcher/src/lib.rs", "pub fn len(&self) -> usize {"),
            generic=[("crates/matcher/src/lib.rs", "        let mut last_end = at;", "        let _bench_edit = 1u32;\n        let mut last_end = at;")],
            # Its Serialize is written out field by field, so the JSON printer says the same.
            struct=field("crates/printer/src/jsont.rs", "pub(crate) struct SubMatch<'a> {", [("crates/printer/src/json.rs", "jsont::SubMatch {")]),
        ),
        test=["--workspace"],
        # The standard library's source, blaze's library/, which its digest pins.
        runtime=dict(bin="rg", args=["-uuu", "--sort", "path", "-c", r"\w+_unchecked\(|unsafe \{", "{library}"]),
    ),
    "cargo": dict(
        repo="rust-lang/cargo", tag="0.92.0", rev="ea2d97820c16195b0ca3fadb4319fe512c199a43",
        run=["--version"],
        edits=dict(
            body=body("src/cargo/ops/cargo_compile/mod.rs", "pub fn compile<'a>(ws: &Workspace<'a>, options: &CompileOptions) -> CargoResult<Compilation<'a>> {"),
            signature=parameter("src/cargo/util/context/mod.rs", "cli_unstable", "&CliUnstable", "src/**/*.rs"),
            leaf=body("crates/cargo-util/src/paths.rs", "pub fn normalize_path(path: &Path) -> PathBuf {"),
            generic=body("src/cargo/util/context/mod.rs", "pub fn get<'de, T: serde::de::Deserialize<'de>>(&self, key: &str) -> CargoResult<T> {"),
        ),
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<ExitCode> {"),
            signature=parameter("src/cli.rs", "max_results", "Option<usize>", "src/**/*.rs"),
            leaf=body("src/dir_entry.rs", "pub fn path(&self) -> &Path {"),
            struct=field("src/filetypes.rs", "pub struct FileTypes {", []),
        ),
        test=[],
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=dict(
            body=body("src/bin/bat/main.rs", "fn run() -> Result<bool> {"),
            signature=parameter("src/assets.rs", "get_theme_set", "&LazyThemeSet", "src/assets.rs"),
            leaf=body("src/assets.rs", "pub fn get_syntax_set(&self) -> Result<&SyntaxSet> {"),
            struct=field("src/input.rs", "pub(crate) struct InputMetadata {", [("src/input.rs", "let metadata = InputMetadata {")]),
        ),
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0", rev="12fec42098642a19855ead34c8cb1e0be28c8ead",
        run=["--help"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<()> {"),
            signature=parameter("src/command.rs", "get_name", "String", "src/**/*.rs"),
            leaf=body("src/command.rs", "pub fn get_command_line(&self) -> String {"),
            struct=[("src/options.rs", "pub struct RunBounds {", "pub struct RunBounds {\n    pub _bench_edit: (),"),
                    ("src/options.rs", "RunBounds { min: 10, max: None }", "RunBounds { min: 10, max: None, _bench_edit: () }")],
        ),
        test=[],
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7",
        run=["--list", "--unsorted"],
        edits=dict(
            body=body("src/run.rs", "pub fn run(args: impl Iterator<Item = impl Into<OsString> + Clone>) -> Result<(), i32> {"),
            signature=parameter("src/lexer.rs", "rest", "&'src str", "src/lexer.rs"),
            leaf=body("src/lexer.rs", "  fn lexeme(&self) -> &'src str {"),
            struct=field("src/execution_context.rs", "pub(crate) struct ExecutionContext<'src: 'run, 'run> {",
                         [("src/*.rs", "let context = ExecutionContext {")]),
        ),
        test=[],
    ),
    # A server: it is built, not run, so nothing checks what it does.
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6", rev="c1bb9c3e90625fb8de4f9c9750f669cb4edac3df",
        build=["-p", "example-hello-world"], run=None,
        edits=dict(
            body=body("examples/hello-world/src/main.rs", "async fn handler() -> Html<&'static str> {"),
            leaf=body("axum-core/src/body.rs", "pub fn empty() -> Self {"),
        ),
        test=["-p", "example-testing"],
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        edits=dict(
            body=body("src/main.rs", "fn main() -> Result<()> {"),
            signature=parameter("crates/nu-protocol/src/value/mod.rs", "coerce_bool", "Result<bool, ShellError>", "crates/**/*.rs"),
            leaf=body("crates/nu-path/src/components.rs", "pub fn components(path: &Path) -> impl Iterator<Item = Component<'_>> {"),
            generic=[("crates/nu-protocol/src/pipeline/pipeline_data.rs", "        F: FnMut(Value) -> Value + 'static + Send,\n    {",
                      "        F: FnMut(Value) -> Value + 'static + Send,\n    {\n        let _bench_edit = 1u32;")],
            # A struct of nu-protocol, which nearly every crate of the workspace depends on.
            struct=field("crates/nu-protocol/src/alias.rs", "pub struct Alias {", [("crates/nu-parser/src/parse_keywords.rs", "let decl = Alias {")]),
        ),
        test=["-p", "nu-path", "-p", "nu-protocol"],
    ),
    "helix": dict(
        repo="helix-editor/helix", tag="25.07.1", rev="a05c151bb6e8e9c65ec390b0ae2afe7a5efd619b",
        # Its build script would otherwise fetch and build every tree-sitter grammar.
        env=dict(HELIX_DISABLE_AUTO_GRAMMAR_BUILD="1"),
        run=["--version"],
        edits=dict(
            body=body("helix-term/src/main.rs", "fn main() -> Result<()> {"),
            signature=parameter("helix-view/src/document.rs", "language_name", "Option<&str>", "helix-*/src/**/*.rs"),
            leaf=body("helix-core/src/chars.rs", "pub fn char_is_word(ch: char) -> bool {"),
            struct=field("helix-term/src/compositor.rs", "pub struct Compositor {",
                         [("helix-term/src/compositor.rs", "pub fn new(area: Rect) -> Self {\n        Self {")]),
        ),
        test=["-p", "helix-core"],
    ),
    # A JavaScript toolchain: 1.24M lines in 104 crates, much of it generated syntax and rules.
    # The program checks one file of its own (parsed, linted and its formatting compared) and
    # reports with the GitHub reporter, which prints no timings. Its syntax, parsers and
    # analyzers are built into its proc macros, so an edit to them is cargo's to build; the
    # edits are in the formatter, the service and the CLI, which only the program is built on.
    "biome": dict(
        repo="biomejs/biome", tag="@biomejs/biome@2.6.3", rev="c5c3383d590d6e08b08d625d10d1d557577284d7",
        build=["-p", "biome_cli", "--bin", "biome"],
        run=["check", "--reporter=github", "--colors=off", "packages/@biomejs/js-api/src/common.ts"],
        edits=dict(
            body=body("crates/biome_cli/src/main.rs", "fn run_workspace(console: &mut EnvConsole, command: BiomeCommand) -> Result<(), CliDiagnostic> {"),
            signature=parameter("crates/biome_service/src/workspace.rs", "supports_format", "bool", "crates/**/*.rs"),
            leaf=body("crates/biome_js_formatter/src/context.rs", "    pub fn quote_style(&self) -> QuoteStyle {"),
            # Instantiated for array expressions, array bindings and array assignment patterns.
            generic=body("crates/biome_js_formatter/src/utils/array.rs", "    let trailing_separator = FormatTrailingCommas::ES5.trailing_separator(f.options());"),
            struct=field("crates/biome_cli/src/lib.rs", "pub struct CliSession<'app> {",
                         [("crates/biome_cli/src/lib.rs", "            app: App::new(console, WorkspaceRef::Borrowed(workspace)),"),
                          ("crates/biome_cli/tests/main.rs", "let session = CliSession {")]),
        ),
        test=["-p", "biome_js_formatter", "--lib"],
    ),
    # 1.7M lines in 249 crates. `--help` is answered before anything opens a window.
    "zed": dict(
        repo="zed-industries/zed", tag="v1.24.4", rev="aed37547d8543000494fb69101c05daec5891068",
        run=["--help"],
        edits=dict(
            body=body("crates/zed/src/main.rs", "fn main() {"),
            signature=parameter("crates/language/src/language.rs", "code_fence_block_name", "Arc<str>", "crates/**/*.rs"),
            leaf=body("crates/util/src/util.rs", "pub fn truncate(s: &str, max_chars: usize) -> &str {"),
            generic=body("crates/gpui_util/src/lib.rs", "    fn log_err(self) -> Option<T> {"),
            struct=field("crates/language/src/language.rs", "pub struct PromptResponseContext {",
                         [("crates/project/src/lsp_store.rs", "let context = language::PromptResponseContext {")]),
        ),
        test=["-p", "util"],
    ),
    # A browser engine. SpiderMonkey comes as mozjs's prebuilt archive; `--version` is answered
    # before anything opens a window. servoprep.py takes stylo's build script out of the
    # sandbox: it writes into the target directory. Measured only when named.
    "servo": dict(
        repo="servo/servo", tag="v0.7.0", rev="aac43a3f31a259f04a574f5ec4e959c943ad7cc7",
        prepare="servoprep",
        build=["-p", "servoshell", "--bin", "servoshell"], run=["--version"],
        edits=dict(
            body=body("ports/servoshell/main.rs", "fn main() {"),
            leaf=body("components/url/lib.rs", "pub fn scheme(&self) -> &str {"),
        ),
        test=["-p", "servo-url"],
    ),
    # Astral's Python package manager, 92 crates of its own; what astral-experiments/blaze
    # tested its fork on. On main 7f4e04dcc8 its cold build fails: getrandom's
    # `getrandom_inner` in the wasm proc macro `const-random-macro` is
    # `UnresolvedMethod("map_err")`.
    "uv": dict(
        repo="astral-sh/uv", tag="0.13.6", rev="329bf9448afd9f9088d14a4d57fbc61eb454f117",
        build=["-p", "uv", "--bin", "uv"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/uv/src/lib.rs", "pub async fn run(cli: Cli, global_initialization: GlobalInitialization) -> Result<ExitStatus> {"),
            leaf=body("crates/uv-normalize/src/package_name.rs", "    pub fn as_str(&self) -> &str {"),
            generic=body("crates/uv-fs/src/path.rs", "pub fn normalize_path<'path>(path: impl Into<Cow<'path, Path>>) -> Cow<'path, Path> {"),
        ),
        test=["-p", "uv-normalize", "-p", "uv-pep440"],
    ),
    # Astral's Python linter and its type checker share one workspace, built a binary each.
    "ruff": dict(
        repo="astral-sh/ruff", tag="0.17.3", rev="ba65abf8ba7bfbcc059f7fb9712be5ba9cab6abc",
        build=["-p", "ruff", "--bin", "ruff"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/ruff/src/main.rs", "fn main() -> ExitCode {"),
            leaf=body("crates/ruff_text_size/src/range.rs", "    pub const fn len(self) -> TextSize {"),
            generic=body("crates/ruff_python_ast/src/visitor.rs", "pub fn walk_stmt<'a, V: Visitor<'a> + ?Sized>(visitor: &mut V, stmt: &'a Stmt) {"),
        ),
        test=["-p", "ruff_text_size", "-p", "ruff_python_parser"],
    ),
    "ty": dict(
        repo="astral-sh/ruff", tag="0.17.3", rev="ba65abf8ba7bfbcc059f7fb9712be5ba9cab6abc",
        build=["-p", "ty", "--bin", "ty"], run=["--version"], named=True,
        edits=dict(
            body=body("crates/ty_python_semantic/src/types.rs", "pub fn check_types(db: &dyn Db, file: ProgramFile<'_>) -> Vec<Diagnostic> {"),
            leaf=body("crates/ruff_text_size/src/range.rs", "    pub const fn len(self) -> TextSize {"),
        ),
        test=["-p", "ty_python_semantic", "--lib"],
    ),
    # 1.25M lines of Rust in 103 crates, with the C and C++ of a JavaScript runtime (bun's port
    # from Zig, fed1eb428d:docs/research/bun.md). Its own build makes the C and C++ once per revision
    # (bunprep.py), and its pinned nightly builds the Rust. Measured only when named.
    "bun": dict(
        repo="oven-sh/bun", tag="bun-v1.4.3", rev="c6da4a4d3010e5553438c60f6bd76d981976867c",
        prepare="bunprep", build=["-p", "bun-exe"], run=["--revision"],
        edits=dict(
            body=body("src/runtime/cli/mod.rs", "pub(crate) fn print_revision_and_exit() -> ! {"),
            signature=parameter("src/runtime/cli/multi_run.rs", "is_done", "bool", "src/runtime/cli/multi_run.rs"),
            leaf=body("src/bun_core/string/StringBuilder.rs", "pub fn append(&mut self, slice: &[u8]) -> &[u8] {"),
            generic=body("src/collections/multi_array_list.rs", "pub(crate) fn push(&mut self, elem: T) -> Result<(), AllocError> {"),
        ),
        # bun's transpiler on five of its own TypeScript files.
        runtime=dict(bin="bun", args=["-e", "const t = new Bun.Transpiler({ loader: 'ts', minifyWhitespace: true }); "
                                      "const fs = require('fs'); const sources = ['http2', 'net', '_http_server', 'fs', 'child_process']"
                                      ".map(n => fs.readFileSync('src/js/node/' + n + '.ts', 'utf8')); let n = 0; "
                                      "for (let i = 0; i < 20; i++) for (const s of sources) n += t.transformSync(s).length; console.log(n)"]),
    ),
    # NVIDIA Dynamo's Rust: the CPU-only vLLM mocker server over dynamo-llm, the runtime, the KV
    # router and the mocker, without a GPU (CUDA is loaded at run time, NIXL is nixl-sys's
    # stubs). Its build scripts need protoc and libclang. cargo builds with the project's pin
    # (rust-toolchain.toml), as its users do: blaze's nightly does not compile dynamo-llm
    # (E0277 in block_manager, measured 2026-10-27).
    "dynamo": dict(
        repo="ai-dynamo/dynamo", tag="v1.6.1", rev="0d9c24a8bf6db13c1a1223cf15fcbee9140d1ac3",
        env=dict(RUSTUP_TOOLCHAIN="1.96.1"), named=True,
        build=["-p", "dynamo-vllm-mocker", "--bin", "dynamo-vllm-mocker-server"], run=["--help"],
        edits=dict(
            body=body("lib/mocker/servers/vllm/src/main.rs", "async fn main() -> anyhow::Result<()> {"),
            leaf=body("lib/tokens/src/lib.rs", "    pub fn remaining(&self) -> usize {"),
        ),
        test=["-p", "dynamo-tokens"],
    ),
    # A file finder (formerly fff.nvim): its MCP server, over the search core, the grep and the
    # query parser. The run is its health check, which prints the paths it is given as given.
    "fff": dict(
        repo="dmtrKovalenko/fff", tag="v0.11.0", rev="95fd777c2529fc7b4d7572dabff64cc07268f2c5",
        build=["-p", "fff-mcp", "--bin", "fff-mcp"],
        run=["--healthcheck", "--frecency-db", "target/bench-fff/frecency", "--history-db", "target/bench-fff/history",
             "--log-file", "target/bench-fff/fff.log", "crates"],
        edits=dict(
            body=body("crates/fff-mcp/src/main.rs", "fn resolve_defaults(args: &mut Args) {"),
            # Defined in the query parser, called from the core, the server and the Neovim binding.
            signature=parameter("crates/fff-query-parser/src/parser.rs", "grep_text", "String", "crates/**/*.rs"),
            leaf=body("crates/fff-grep/src/matcher.rs", "    pub fn len(&self) -> usize {"),
            # Instantiated for each ParserConfig (the file picker's, grep's, the default).
            generic=body("crates/fff-query-parser/src/parser.rs",
                         "fn parse_token<'a, C: ParserConfig>(token: &'a str, config: &C) -> Option<Constraint<'a>> {"),
        ),
        test=["-p", "fff-query-parser", "-p", "fff-grep", "-p", "fff-search"],
    ),
    # A video rendering framework: its hello-world example's renderer, over FFmpeg and Skia,
    # which its build scripts download (a prebuilt Skia; FFmpeg's source for the example's
    # codecs, a prebuilt FFmpeg for the tests'). v1 locked those downloads with `blaze lock
    # --fetches`; v2 has no such command, so its sandboxed build scripts are refused the network
    # until it has. The run is one frame by the CPU backend. Its build scripts need nasm and
    # libclang: measured only when named.
    "fframes": dict(
        repo="dmtrKovalenko/fframes", tag="v1.3.0", rev="69174f0187b5d8e92329246898f70250b59f08f2", named=True,
        build=["-p", "hello-world-example", "--bin", "hello_world_example_bin"],
        run=["--renderer", "cpu", "svg", "1s"],
        edits=dict(
            body=body("examples/hello-world/src/main.rs", "fn main() -> ExitCode {"),
            # fframes-media is below fframes and its media proc macro.
            leaf=body("fframes-media/src/audio.rs", "    pub fn duration_in_seconds(&self) -> f32 {"),
            generic=body("fframes/src/fframes_context.rs",
                         "    pub fn get_scene_info<T: crate::Scene>(&self, scene: &T) -> Option<&crate::SceneInfo> {"),
        ),
        test=["-p", "fframes", "-p", "fframes-media", "-p", "webvtt-parser", "-p", "fframes-media-dir-macro"],
    ),
    # The environment manager built on Nix: its CLI over the SDK, the manifest and the catalog
    # client. Its crates read Nix store paths at compile time, which floxprep.py takes from its
    # flake: measured only when named.
    "flox": dict(
        repo="flox/flox", tag="v1.19.2", rev="87d931382dc5c53f32af662c9611c6a58d669952",
        prepare="floxprep", env=dict(FLOX_DISABLE_METRICS="true"),
        build=["-p", "flox", "--bin", "flox"], run=["--version"],
        edits=dict(
            body=body("cli/flox/src/main.rs", "fn main() -> ExitCode {"),
            # Defined in the manifest crate, called from the CLI's `upgrade`.
            signature=parameter("cli/flox-manifest/src/parsed/v1_10_0/package_descriptor.rs", "package_identifier", "&str", "cli/**/*.rs"),
            leaf=body("cli/flox-core/src/floxhub.rs", "    pub fn api_url_str(&self) -> String {"),
            generic=[("cli/flox-rust-sdk/src/utils/mod.rs", "    let mut serialized = serde_json::to_string_pretty(value)?;",
                      "    let _bench_edit = 1u32;\n    let mut serialized = serde_json::to_string_pretty(value)?;")],
        ),
        test=["-p", "flox-core", "-p", "flox-config", "--features", "flox-core/tests"],
    ),
    # OpenAI's Codex CLI: its Rust workspace is codex-rs (`dir`), 162 members; the `codex`
    # binary is over 151 of its 167 local packages and about 1,200 packages in all. cargo builds with its pin (rust-toolchain.toml, 1.99.1), as its users do.
    # Needs libcap (its Linux sandbox) and ALSA; measured only when named.
    "codex": dict(
        repo="openai/codex", tag="rust-v0.165.0", rev="360319d8dee62d87e2d59fc0c21805d79519cfed",
        dir="codex-rs", env=dict(RUSTUP_TOOLCHAIN="1.99.1"), named=True,
        build=["-p", "codex-cli", "--bin", "codex"], run=["--version"],
        edits=dict(
            body=body("cli/src/main.rs", "fn main() -> anyhow::Result<()> {"),
            # codex-protocol, which most crates of the workspace depend on.
            leaf=body("protocol/src/protocol.rs", "pub fn strip_user_message_prefix(text: &str) -> &str {"),
            # AbsolutePathBuf::join, which the workspace calls at many types of path.
            generic=body("utils/absolute-path/src/lib.rs", "    pub fn join<P: AsRef<Path>>(&self, path: P) -> Self {"),
        ),
        test=["-p", "codex-apply-patch", "-p", "codex-protocol", "-p", "codex-execpolicy", "-p", "codex-features"],
    ),
    # A game: built, not run (it opens a window). Its own code is small; the engine is ~400 crates.
    "bevy": dict(
        repo="bevyengine/bevy", tag="v0.20.0", rev="b3bdd75becaffe9a16677cf5bae5bd6d198f052b",
        build=["--example", "breakout"], run=None,
        edits=dict(
            body=body("examples/showcase/breakout.rs", "fn apply_velocity(mut query: Query<(&mut Transform, &Velocity)>, time: Res<Time>) {"),
            leaf=body("crates/bevy_math/src/rects/rect.rs", "pub fn as_irect(&self) -> IRect {"),
        ),
    ),
}

# Every project with an edit of a function's body also gets a new function, called from it.
for _spec in PROJECTS.values():
    if "body" in _spec["edits"] and "newfn" not in _spec["edits"]:
        (_file, _opening, _), = _spec["edits"]["body"]
        _spec["edits"]["newfn"] = new_function(_file, _opening)

# Each backend as v2 picks it, by the profile. v2 compiles dev with Cranelift and release with LLVM, and refuses every unit
# while BLAZE_BACKEND or BLAZE_OPT is set (crates/hir-ty-codegen/v2/backend.rs).
BACKENDS = {"cranelift": (), "llvm": ("--release",)}
DURATION = re.compile(r"([0-9.]+)(ns|µs|ms|s)\b")
UNITS = {"ns": 1e-9, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
FLOOR_SECS = 0.05
RESULT_SCHEMA = benchschema.VERSION


@dataclass(frozen=True)
class Run:
    blaze: str
    work: str
    out: str
    action: Literal["run"] = "run"
    projects: tuple[str, ...] = ()
    backends: tuple[Literal["cranelift", "llvm"], ...] = ("cranelift", "llvm")
    sources: str = os.path.expanduser("~/blaze-projects")
    runs: int = 3
    cold_runs: int = 1
    tests: bool = False
    stress: bool = False
    keep: bool = False
    machine: str | None = None
    commit: str | None = None
    run_url: str | None = None

    def __post_init__(self):
        for name in ("blaze", "work", "out", "sources"):
            if not getattr(self, name):
                raise ValueError(f"{name} must name a path")
        for name in ("runs", "cold_runs"):
            if getattr(self, name) < 1:
                raise ValueError(f"{name} must be positive")
        if not self.backends or len(set(self.backends)) != len(self.backends):
            raise ValueError("backends must be a nonempty list without repeats")
        if len(set(self.projects)) != len(self.projects):
            raise ValueError("projects must not repeat a checkout")
        if unknown := set(self.projects) - PROJECTS.keys():
            raise ValueError(f"unknown projects: {', '.join(sorted(unknown))}")


@dataclass(frozen=True)
class Compare:
    baseline: str
    result: str
    action: Literal["compare"] = "compare"
    threshold: int | float = 25

    def __post_init__(self):
        if not self.baseline or not self.result:
            raise ValueError("baseline and result must name files")
        if not math.isfinite(self.threshold) or self.threshold < 0:
            raise ValueError("threshold must be a finite nonnegative percent")


def load(text):
    return benchschema.load_input(text, {"run": Run, "compare": Compare}, "run", "bench.py")


def blaze_of(backend):
    """The blaze `backend` is measured with: the one blaze for both, a backend being a profile."""
    del backend
    return blaze_cli.BLAZE


def seconds(text):
    """The seconds a duration as Rust prints it stands for."""
    number, unit = DURATION.search(text).groups()
    return float(number) * UNITS[unit]


def write_edit(ws, changes, back):
    """Makes every change of an edit, or takes it back. The result is how many places changed.
    A change whose old text is None appends its new text to the end of the file."""
    places = 0
    for pattern, old, new in changes:
        if old is None:
            path = os.path.join(ws, pattern)
            text = open(path).read()
            if back:
                assert text.endswith(new), f"{path} does not end with what was appended"
                text = text[:-len(new)]
            else:
                text += new
            open(path, "w").write(text)
            places += 1
            continue
        old, new = (new, old) if back else (old, new)
        for path in sorted(glob.glob(os.path.join(ws, pattern), recursive=True)):
            text = open(path).read()
            if old in text:
                places += text.count(old)
                open(path, "w").write(text.replace(old, new))
    assert places >= len(changes), f"an edit found only {places} places: {changes}"
    return places


class Daemon:
    """The daemon of one checkout's home: its pid, from its lock (`storelib.daemon_pid`), and
    its memory. v2's daemon does the work itself: it has no workers and no worker logs, and
    what it did for a build is that build's `blaze-work` line (`storelib.worked`)."""

    def __init__(self, side):
        self.side, self.peak, self.sampled = side, 0, 0

    def pid(self):
        return storelib.daemon_pid(self.side.env["BLAZE_HOME"])

    def resident_mb(self):
        """What the daemon and every process below it hold now."""
        pid = self.pid()
        return memory.resident_mb([pid]) if pid else 0

    def workers(self):
        """v1's workers, of which v2 has none: kept for the harnesses that still ask."""
        return []

    def peak_mb(self):
        """The most memory the daemon has held so far: its VmHWM, or where there is none
        (macOS) the sampled peak of the daemon's tree."""
        pid = self.pid()
        if not os.path.isdir("/proc/self") or not pid:
            self.peak = max(self.peak, self.sampled)
            return self.peak
        try:
            status = open(f"/proc/{pid}/status").read()
        except OSError:
            return self.peak
        self.peak = max(self.peak, round(int(re.search(r"VmHWM:\s+(\d+) kB", status)[1]) / 1024))
        return self.peak

    def said(self, builds=0):
        """v1's worker logs, of which v2 has none: always empty, kept for the harnesses that
        still ask. What a build did is its own `blaze-work` line."""
        del builds
        return ""


def metered(side, args, daemon=None, sampler=None, command="build", meter=None):
    """The selected Blaze command, timed with the memory of its process tree and daemon.
    `peak_mb` is the most all of them
    held at once, `largest_mb` the largest one process held. With `meter` (counters.Meter) the
    command, and what it starts, is counted by it; the caller reads the meter around it."""
    own = sampler is None
    sampler = sampler or memory.Sampler()
    if daemon:
        sampler.add(daemon.pid())
    if own:
        sampler.start()
    with side.session() as env:
        took, ran, used = memory.run(side.argv(command, *args), side.ws, env, 7200, sampler=sampler, meter=meter)
    # As `Side.cargo` reads it: `side.argv` added the format its build is read by.
    blaze_cli.read_build(ran, cut=side.blaze and bool(blaze_cli.machine_format(command, args)))
    if own:
        sampler.stop()
    used = dict(peak_mb=max(sampler.peak_mb, used["largest_mb"]), largest_mb=max(used["largest_mb"], sampler.largest_mb))
    if daemon:
        daemon.sampled = max(daemon.sampled, used["peak_mb"])
    return took, ran, used


def memory_phase(rows, resident=None):
    """Blaze's median and maximum peaks, and the median of its largest process.
    `resident` is what the daemon held once the phase was over."""
    def side(used):
        peaks = [u["peak_mb"] for u in used]
        return dict(peak_mb=round(statistics.median(peaks)), peak_max_mb=max(peaks),
                    largest_mb=round(statistics.median(u["largest_mb"] for u in used)))
    result = {"blaze": side(rows)}
    if resident is not None:
        result["blaze"]["resident_mb"] = resident
    return result


# What `blaze build --message-format=json` adds to a build's command line: its `blaze-work`
# line (crates/blaze-ui/src/api.rs, `work_json`) says how each image was linked, how many
# functions were compiled and what each step took.
JSON = "--message-format=json"


def links(out):
    """Each image the builds in `out` (what blaze printed with `--message-format=json`)
    linked, as `storelib.Link`s, from their `blaze-work` lines."""
    return [link for work in storelib.worked(out) for link in work.links]


def link_kind(out, linked=frozenset()):
    """How the builds in `out` reached the program: "spliced" when each link put what changed
    into the image kept or left an image nothing it is made of changed ("unchanged"); "first
    link" when an image new to the build was laid out and the rest were spliced; else "full
    link: <why>". `linked` holds the paths of the images linked before `out`: one of them laid
    out as a first link lost the image kept, which is a full link too.

    >>> line = lambda *made: json.dumps(dict(reason="blaze-work", reran=0, green=0, compiled=0, found=0, link="patched",
    ...                                      links=[dict(path=path, how=how, why=why) for path, how, why in made]))
    >>> link_kind("")
    >>> link_kind(line(("target/debug/deps/app-1", "spliced", [])))
    'spliced'
    >>> link_kind(line(("target/debug/deps/app-1", "first", [])))
    'first link'
    >>> link_kind(line(("target/debug/deps/app-1", "first", [])), {"target/debug/deps/app-1"})
    'full link: target/debug/deps/app-1 was linked before and its image was not kept'
    >>> link_kind(line(("a", "spliced", []), ("b", "unchanged", [])))
    'spliced'
    >>> link_kind(line(("a", "spliced", []), ("b", "full", ["exports", "debug"])))
    'full link: b: exports, debug'
    >>> failed_link("full link: b: exports, debug"), failed_link("first link"), failed_link(None)
    (True, False, False)
    """
    found = links(out)
    if not found:
        return None
    for link in found:
        if link.how == "first" and link.path in linked:
            return f"full link: {link.path} was linked before and its image was not kept"
        if link.how not in ("spliced", "unchanged", "first"):
            return f"full link: {link.path}" + (f": {', '.join(link.why)[:160]}" if link.why else f" ({link.how[:160]})")
    if all(link.how in ("spliced", "unchanged") for link in found):
        return "spliced"
    return "first link"


def failed_link(kind):
    """Whether an edit whose links were `kind` ([`link_kind`]) failed: one that laid an image
    out whole again instead of splicing is a finding, never a time (AGENTS.md, no-fallback)."""
    return kind is not None and kind.startswith("full link")


def work_steps(out, scale):
    """What each step of the builds in `out` took, from their `blaze-work` lines' `steps_us`
    (the run time of the request's nodes by kind), summed over the builds, times `scale`."""
    steps = {}
    for message in storelib.messages(out, "blaze-work"):
        took = message.get("steps_us")
        for name, micros in (took.items() if isinstance(took, dict) else ()):
            if isinstance(micros, int):
                steps[name] = steps.get(name, 0) + micros * scale
    return steps


def daemon_steps(out):
    """The steps the daemon timed for the builds in `out`, in milliseconds, summed."""
    return {name: round(ms, 2) for name, ms in work_steps(out, 1e-3).items()}


def cold_steps(out):
    """The steps of the first build in `out`, in seconds."""
    return {name: round(s, 3) for name, s in work_steps(out, 1e-6).items()}


def thread_steps(text):
    """What the daemon's threads spent on each producer in `text`: seconds summed over the
    threads. v1's `backend:` lines of each producer were its workers' logs, which v2 has none
    of, so this is empty until v2 says it."""
    del text
    return {}


def compiled(out):
    """How many functions the builds in `out` compiled (`blaze-work`'s `compiled`)."""
    return sum(work.compiled for work in storelib.worked(out))


def disk(ours):
    """Blaze's target, store and remaining home in MB, as memory.disk_mb counts them."""
    store = memory.disk_mb(storelib.store_of(ours.env["BLAZE_HOME"]))
    return {"target": memory.disk_mb(ours.env["CARGO_TARGET_DIR"]), "store": store,
            "home": memory.disk_mb(ours.env["BLAZE_HOME"]) - store}


def store_mb(path):
    total = 0
    for root, _, files in os.walk(path):
        for file in files:
            try:
                total += os.lstat(os.path.join(root, file)).st_size
            except OSError:
                pass
    return round(total / 1e6)


class Failed(Exception):
    """A build of a project failed: the project ends there, the run goes on."""


def require_build(ran, label):
    """A successful measurement has a successful native daemon build verdict."""
    built = how_built(ran)
    if ran.returncode != 0 or built != "daemon":
        raise Failed(f"{label}: Blaze exited {ran.returncode}; {built}:\n{ran.stderr[-3000:]}")
    return built


def stop(side):
    _, ran = side.cargo("stop", timeout=120)
    if ran.returncode:
        raise Failed(f"stop {side.env['BLAZE_HOME']}: exited {ran.returncode}: {ran.stderr[-1500:]}")


# The phases measured so far and the daemon's side, by (project, backend): what is kept of a
# project that failed.
SO_FAR = {}


def measure(name, spec, backend, runs, work, cold_runs=1, tests=False):
    """Runs every phase of one project under one backend. The result is the phases by name."""
    blaze = blaze_of(backend)
    ws = os.path.realpath(os.path.join(work, f"{name}-{backend}"))
    source, prepared = fetch(name, spec), {}
    if "prepare" in spec:
        # A tree with the project's native prerequisites, and what every build of
        # it needs: its builds read that from the tree, so its `build` is not copied.
        source, prepared = importlib.import_module(spec["prepare"]).prepare(source)
    left_out = lambda at, names: [n for n in names if n in ("target", "target-ref", ".git")
                                  or ("prepare" in spec and n == "build" and os.path.samefile(at, source))]
    shutil.copytree(source, ws, symlinks=True, ignore=left_out)
    # The checkout is copied whole; the workspace is its `dir` (codex's is codex-rs).
    top, ws = ws, os.path.join(ws, spec["dir"]) if "dir" in spec else ws
    home = lambda label: os.path.join(work, f"home-{name}-{backend}-{label}")
    env = dict(blaze_cli.base_env(home(0)), **spec.get("env", {}), **prepared)
    env["PATH"] = os.path.dirname(blaze) + os.pathsep + env["PATH"]
    env.update(BLAZE_STATS="1")
    # A cold Blaze build has its own empty store and target.
    env.pop("RUSTC_WRAPPER", None)
    env.update(BLAZE_HOME=home(0), CARGO_TARGET_DIR=os.path.join(ws, "target"), BLAZE_CACHE=storelib.store_of(home(0)))
    env[blaze_cli.COMPILER_ENV] = os.path.abspath(blaze)
    ours = Side(ws, env, True)
    phases, wrong = {}, []
    # Counts blaze's commands and the daemon the first of them starts (counters.py), for the
    # whole project: each step is read from quiet to quiet (`Meter.settle`).
    meter = counters.Meter()
    SO_FAR[name, backend] = (phases, wrong, ours, meter)
    build, run = [*spec.get("build", []), *BACKENDS[backend]], spec.get("run")
    # Dependency resolution and downloads belong to the selected Blaze cold build.
    def build_once():
        before = meter.settle()
        took, mine, used = metered(ours, [*build, JSON], daemon, meter=meter)
        counted = meter.since(before, meter.settle())
        builds[0] += 1
        built = require_build(mine, f"{name} {backend} build")
        return took, built, used, mine.stdout, counted

    def edit_link(kind, step, out):
        """How the build of an edit linked, from the `blaze-work` lines in what it printed,
        `out`. One that laid an image out whole again, or linked nothing, ends the project's
        run there, before the step is timed."""
        link = link_kind(out, frozenset(linked))
        if link is None:
            raise Failed(f"{name} {backend}: the {kind} {step} has no link in its `blaze-work` line: how it linked is unknown")
        if failed_link(link):
            raise Failed(f"{name} {backend}: the {kind} {step} was not spliced: {link}")
        linked.update(link.path for link in links(out))
        return link

    def program():
        """What blaze's build of the program says, run without `BLAZE_STATS`: its time table
        on stderr differs from run to run and is not the program's."""
        quiet = Side(ws, {k: v for k, v in ours.env.items() if k != "BLAZE_STATS"}, True)
        ran = quiet.cargo("run", *build, "--", *run)[1]
        require_build(ran, f"{name} {backend} run")
        return ran

    def runs_right(label):
        if run is None:
            return
        mine = program()
        if program_output(mine) != expected:
            wrong.append(f"{label}: the program says {program_output(mine)!r}")

    def report(label, phase):
        phases[label] = phase
        held = phase.get("memory") or {}
        mem = (f"peak blaze {held['blaze']['peak_mb']:6} MB  " +
               (f"daemon {held['blaze']['resident_mb']:6} MB  " if "resident_mb" in held["blaze"] else "")) if held else ""
        counted = f"{phase['instructions'] / 1e9:8.3f} G instructions  " if phase.get("instructions") is not None else ""
        print(f"  {label:10} blaze {phase['wall_s']:8.3f}s  {counted}{mem}"
              f"{phase['built'][:60]}  {phase.get('link') or ''}", flush=True)

    def cold_blaze(side, label, watched, counting):
        """`blaze build` from nothing on `side`, with the memory of both (the daemon is the
        command's child when it starts, so the sampler finds it), what the daemon holds after
        it, and what the command and the daemon retired until it was quiet (by the meter
        `counting`). v2's first build is the daemon's, with nothing left to load after it, so
        the wait for it to be ready (`warm`) is 0."""
        before = counting.snapshot()
        with memory.Sampler() as sampler:
            took, mine, used = metered(side, [*build, JSON], sampler=sampler, meter=counting)
            require_build(mine, f"{name} {backend} first build ({label})")
        warm = 0.0
        used = dict(peak_mb=max(sampler.peak_mb, used["largest_mb"]), largest_mb=max(used["largest_mb"], sampler.largest_mb),
                    resident_mb=watched.resident_mb())
        watched.sampled = max(watched.sampled, used["peak_mb"])
        if pid := watched.pid():
            # macOS: the daemon left the command's tree; Linux's counter already follows it.
            counting.adopt(pid)
        return took, warm, mine, used, counting.since(before, counting.settle())

    # Each cold run starts from nothing: no target directories, an empty store and a daemon of
    # its own. The last one's daemon is the one the edits are measured with.
    colds, builds = [], [0]
    for turn in range(cold_runs):
        stop(ours)
        for path in (ours.env["CARGO_TARGET_DIR"], home(turn)):
            shutil.rmtree(path, ignore_errors=True)
        ours.env.update(BLAZE_HOME=home(turn), BLAZE_CACHE=storelib.store_of(home(turn)))
        daemon = Daemon(ours)
        took, warm, mine, used, counted = cold_blaze(ours, "empty store", daemon, meter)
        said, built, peak = mine.stdout, how_built(mine), daemon.peak_mb()
        # The images this daemon has linked: each later link of one is an edit of it.
        linked = {link.path for link in links(said)}
        colds.append(dict(build_s=took, warm_s=warm, used=used, counts=counted))
        print(f"  cold {turn + 1}/{cold_runs}  blaze {took:.2f}s + ready {warm:.2f}s", flush=True)
        if turn + 1 < cold_runs:
            stop(ours)
            shutil.rmtree(home(turn), ignore_errors=True)
    ready_mb = round(statistics.median(c["used"]["resident_mb"] for c in colds))
    resident = dict(ready_mb=ready_mb, after=[])
    cold_disk = disk(ours)
    expected = program_output(program()) if run is not None else None
    med = lambda key: round(statistics.median(c[key] for c in colds), 3)
    report("cold", dict(wall_s=round(statistics.median(c["build_s"] + c["warm_s"] for c in colds), 3),
                        rss_mb=max(peak, daemon.peak_mb()), built=built, build_s=med("build_s"), warm_s=med("warm_s"),
                        samples_s=[round(c["build_s"] + c["warm_s"], 3) for c in colds],
                        build_samples_s=[round(c["build_s"], 3) for c in colds],
                        compiled=compiled(said), steps_s=cold_steps(said), threads_s=thread_steps(said),
                        memory=memory_phase([c["used"] for c in colds], ready_mb), disk_mb=cold_disk,
                        **counters.summary([c["counts"] for c in colds])))

    samples = [build_once() for _ in range(max(runs, 3))]
    said = "".join(s[3] for s in samples)
    linked.update(link.path for link in links(said))
    held = daemon.resident_mb()
    resident["after"].append(dict(phase="no-op", builds=builds[0], mb=held))
    report("no-op", dict(memory=memory_phase([s[2] for s in samples], held), wall_s=round(statistics.median(s[0] for s in samples), 3),
                         rss_mb=daemon.peak_mb(), built=samples[-1][1], compiled=compiled(said),
                         samples_s=[round(s[0], 3) for s in samples], **counters.summary([s[4] for s in samples])))
    runs_right("no-op")

    for kind, changes in spec["edits"].items():
        made, back = [], []
        for turn in range(runs):
            places = write_edit(ws, changes, False)
            took, built, used, said, counted = build_once()
            link = edit_link(kind, "edit", said)
            made.append(dict(wall_s=took, built=built, link=link, compiled=compiled(said), steps_ms=daemon_steps(said),
                             threads_s=thread_steps(said), used=used, counts=counted))
            if turn == 0:
                runs_right(kind)
            write_edit(ws, changes, True)
            took, built, _, said, _ = build_once()
            link = edit_link(kind, "edit taken back", said)
            back.append(dict(wall_s=took, built=built, link=link, compiled=compiled(said)))
        # The slowest of them is the one to look at; the median is the one to compare.
        slowest = max(made, key=lambda s: s["wall_s"])
        median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
        held = daemon.resident_mb()
        resident["after"].append(dict(phase=kind, builds=builds[0], mb=held))
        report(kind, dict(memory=memory_phase([s["used"] for s in made], held), wall_s=median(made, "wall_s"), rss_mb=daemon.peak_mb(),
                          built=slowest["built"], link=slowest["link"], compiled=max(s["compiled"] for s in made), places=places,
                          steps_ms=slowest["steps_ms"], threads_s=slowest["threads_s"], slowest_s=round(slowest["wall_s"], 3),
                          back_s=median(back, "wall_s"),
                          back_link=max(back, key=lambda s: s["wall_s"])["link"],
                          samples_s=[round(s["wall_s"], 3) for s in made], **counters.summary([s["counts"] for s in made])))
    resident.update(end_mb=resident["after"][-1]["mb"], builds=builds[0])
    end_disk = disk(ours)
    if tests and "test" in spec:
        phases["test"] = test_suite(name, spec, ours, ws, wrong)
        phases["test"].update(instructions=None, cycles=None,
                              counts_gap="`blaze test` runs through blaze_cli.Side, outside memory.run: not counted")
    stop(ours)
    # Other checkouts of the same commit, each with a daemon of its own and an empty target
    # directory but the store the measured one filled: a second checkout, or a teammate's store.
    warm_store = []
    for turn in range(cold_runs):
        other_top = top + "-b"
        shutil.copytree(top, other_top, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", "target-*"))
        other_ws = os.path.realpath(os.path.join(other_top, os.path.relpath(ws, top)))
        shutil.rmtree(home(f"{turn}b"), ignore_errors=True)
        shutil.copytree(storelib.store_of(ours.env["BLAZE_HOME"]), storelib.store_of(home(f"{turn}b")), symlinks=True)
        second = Side(other_ws, dict(ours.env, BLAZE_HOME=home(f"{turn}b"),
                                    BLAZE_CACHE=storelib.store_of(home(f"{turn}b")),
                                    CARGO_TARGET_DIR=os.path.join(other_ws, "target")), True)
        try:
            with counters.Meter() as other_meter:
                took_b, warm_b, _, used_b, counted_b = cold_blaze(second, "store filled by another checkout", Daemon(second), other_meter)
        finally:
            stop(second)
        shutil.rmtree(other_top, ignore_errors=True)
        shutil.rmtree(home(f"{turn}b"), ignore_errors=True)
        warm_store.append((took_b, warm_b, used_b, counted_b))
        print(f"  warm store {turn + 1}/{cold_runs}  blaze {took_b:.2f}s + ready {warm_b:.2f}s  peak {used_b['peak_mb']} MB  "
              f"daemon {used_b['resident_mb']} MB", flush=True)
    if warm_store:
        phases["cold"].update(warm_store_build_s=round(statistics.median(b for b, *_ in warm_store), 3),
                              warm_store_s=round(statistics.median(b + w for b, w, *_ in warm_store), 3),
                              warm_store_samples_s=[round(b + w, 3) for b, w, *_ in warm_store],
                              warm_store_memory=dict(peak_mb=round(statistics.median(u["peak_mb"] for _, _, u, _ in warm_store)),
                                                     resident_mb=round(statistics.median(u["resident_mb"] for _, _, u, _ in warm_store))),
                              warm_store_instructions=counters.summary([c for *_, c in warm_store])["instructions"])
    stored = store_mb(storelib.store_of(ours.env["BLAZE_HOME"]))
    print(f"  store {stored} MB", flush=True)
    meter.close()
    result = dict(phases=phases, wrong=wrong, tag=spec["tag"], rev=spec["rev"], store_mb=stored,
                  cold_runs=cold_runs, resident=resident, disk_mb=end_disk)
    if "runtime" in spec:
        result["runtime"] = runtime(name, spec, ours, ws, build, max(runs, 10), wrong)
    return result


def test_suite(name, spec, ours, ws, wrong):
    """The project's own tests under the selected Blaze, before/after edits and with no change.
    Three initial runs are retained separately. Known failures stay explicit in test_known.
    Every verdict, including the reversal, must pass that rule."""
    args, edit = spec["test"], spec["edits"].get("leaf") or spec["edits"]["body"]

    def tests(label):
        took, mine = ours.cargo("test", *args)
        said = test_results(mine)
        differs = test_verdict(said, frozenset(spec.get("test_known", ())))
        built = how_built(mine)
        if built != "daemon":
            differs = f"{built}; {differs}" if differs else built
        if differs:
            wrong.append(f"tests ({label}): {differs}")
        return dict(wall_s=took, built=built, said=said, differs=differs)

    first = [tests("first runs") for _ in range(3)]
    edited, still = [], []
    for _ in range(3):
        write_edit(ws, edit, False)
        edited.append(tests("after an edit"))
        write_edit(ws, edit, True)
        tests("the edit taken back")
        still.append(tests("nothing changed"))
    median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
    last = still[-1]
    counted = lambda said: sum(n for (_, outcome), n in said[1].items() if outcome == "ran")
    failed = lambda said: sum(n for (_, outcome), n in said[1].items() if outcome == "FAILED")
    differs = next((r["differs"] for r in first + edited + still if r["differs"]), None)
    slowest = max(edited, key=lambda r: r["wall_s"])
    phase = dict(wall_s=median(still, "wall_s"), rss_mb=0, built=slowest["built"],
                 built_still=max(still, key=lambda r: r["wall_s"])["built"],
                 edit_s=median(edited, "wall_s"),
                 first_s=[round(r["wall_s"], 3) for r in first], samples_s=[round(r["wall_s"], 3) for r in still],
                 edit_samples_s=[round(r["wall_s"], 3) for r in edited],
                 command=["test", *args], edited=edit[0][0], tests=counted(last["said"]), failed=failed(last["said"]),
                 failed_blaze=failed(last["said"]), same=differs is None, difference=differs)
    print(f"  test       after an edit: blaze {phase['edit_s']:8.3f}s ({phase['built'][:40]})  "
          f"nothing changed: blaze {phase['wall_s']:8.3f}s  first runs {phase['first_s']}  "
          f"{phase['tests']} tests, {'all pass' if differs is None else 'WRONG: ' + differs[:200]}", flush=True)
    return phase


# The stress corpus: small crates, each a pathological shape for a compiler, named with what
# each is for by its manifest. Measured cold when stress is true.
STRESS = os.path.join(HERE, "stress-corpus")
STRESS_MANIFEST = os.path.join(STRESS, "manifest.json")


def stress_crates():
    """The crates of harness/stress-corpus/manifest.json, `{"version": 1, "crates": [{"name",
    "path" (relative to harness/stress-corpus), "what"}]}`, and None; or no crates and why there
    are none, when the manifest is not in this tree."""
    if not os.path.exists(STRESS_MANIFEST):
        return [], f"no {os.path.relpath(STRESS_MANIFEST, ROOT)} in this tree: the stress phase measured nothing"
    manifest = json.load(open(STRESS_MANIFEST))
    if not isinstance(manifest, dict) or manifest.get("version") != 1:
        version = manifest.get("version") if isinstance(manifest, dict) else manifest
        raise SystemExit(f"{STRESS_MANIFEST}: this reads version 1, not {version!r}")
    crates = manifest.get("crates")
    if not isinstance(crates, list):
        raise SystemExit(f"{STRESS_MANIFEST}: `crates` is not a list")
    for at, crate in enumerate(crates):
        if not isinstance(crate, dict):
            raise SystemExit(f"{STRESS_MANIFEST}: crates[{at}] is not an object")
        bad = [k for k in ("name", "path", "what") if not isinstance(crate.get(k), str)]
        extra = sorted(set(crate) - {"name", "path", "what"})
        if bad or extra:
            raise SystemExit(f"{STRESS_MANIFEST}: crates[{at}] has the strings name, path and what and nothing else"
                             + (f"; missing or not a string: {', '.join(bad)}" if bad else "")
                             + (f"; unknown: {', '.join(extra)}" if extra else ""))
        if not os.path.isfile(os.path.join(STRESS, crate["path"], "Cargo.toml")):
            raise SystemExit(f"{STRESS_MANIFEST}: crates[{at}] ({crate['name']}): no Cargo.toml in {crate['path']}")
    return crates, None


def measure_stress(crate, backend, runs, work):
    """A cold `blaze check`, then a cold `blaze build`, of one stress crate, `runs` times each:
    each from nothing (no target directory, an empty home and store, no daemon), until the daemon
    is ready and quiet. Each phase has the median wall time, blaze's peak memory and the
    instructions and cycles of the command and the daemon it started (counters.py)."""
    name = crate["name"]
    ws = os.path.realpath(os.path.join(work, f"stress-{name}-{backend}"))
    shutil.rmtree(ws, ignore_errors=True)
    shutil.copytree(os.path.join(STRESS, crate["path"]), ws, symlinks=True, ignore=shutil.ignore_patterns("target"))
    # A workspace of its own, wherever it is copied to.
    toml = os.path.join(ws, "Cargo.toml")
    text = open(toml).read()
    if not re.search(r"^\[workspace\]", text, re.M):
        open(toml, "w").write(text + "\n[workspace]\n")
    home = os.path.join(work, f"home-stress-{name}-{backend}")
    env = blaze_cli.base_env(home)
    env["PATH"] = os.path.dirname(blaze_of(backend)) + os.pathsep + env["PATH"]
    env.update(BLAZE_STATS="1", BLAZE_CACHE=storelib.store_of(home), CARGO_TARGET_DIR=os.path.join(ws, "target"))
    env.pop("RUSTC_WRAPPER", None)
    side = Side(ws, env, True)
    args = list(BACKENDS[backend])
    phases = {}
    for command in ("check", "build"):
        rows = []
        for _ in range(runs):
            stop(side)
            for path in (os.path.join(ws, "target"), home):
                shutil.rmtree(path, ignore_errors=True)
            with counters.Meter() as meter:
                before = meter.snapshot()
                with memory.Sampler() as sampler:
                    took, ran, used = metered(side, args, sampler=sampler, command=command, meter=meter)
                    try:
                        require_build(ran, f"stress {name} {backend} cold {command}")
                    except Failed:
                        stop(side)
                        raise
                # v2's first build is the daemon's, with nothing left to load after it.
                warm = 0.0
                if pid := Daemon(side).pid():
                    meter.adopt(pid)
                counted = meter.since(before, meter.settle())
            stop(side)
            rows.append(dict(wall_s=took + warm, build_s=took, warm_s=warm, peak_mb=max(sampler.peak_mb, used["largest_mb"]),
                             built=how_built(ran), counts=counted))
        median = lambda key: round(statistics.median(r[key] for r in rows), 3)
        peaks = [r["peak_mb"] for r in rows]
        phase = dict(wall_s=median("wall_s"), build_s=median("build_s"), warm_s=median("warm_s"),
                     samples_s=[round(r["wall_s"], 3) for r in rows], built=max(rows, key=lambda r: r["wall_s"])["built"],
                     memory=dict(blaze=dict(peak_mb=round(statistics.median(peaks)), peak_max_mb=max(peaks))),
                     **counters.summary([r["counts"] for r in rows]))
        phases[command] = phase
        counted = (f"{phase['instructions'] / 1e9:.3f} G instructions" if phase["instructions"] is not None
                   else phase.get("counts_gap"))
        print(f"  stress {name} {command:5} {phase['wall_s']:8.3f}s  peak {phase['memory']['blaze']['peak_mb']} MB  {counted}",
              flush=True)
    return dict(phases=phases, wrong=[], tag="stress", path=crate["path"], what=crate["what"], runs=runs)


def library():
    """The source of the standard library blaze compiles, `library/` of this checkout, which its
    digest pins: no toolchain's `rust-src` is read (docs/toolchain.md)."""
    return os.path.join(ROOT, "library")


def runtime(name, spec, ours, ws, build, runs, wrong):
    """The selected profile's Blaze program, after two warmups. Every invocation must exit 0."""
    spec = spec["runtime"]
    built = "release" if "--release" in build else "debug"
    programs = {"blaze": os.path.join(ours.env["CARGO_TARGET_DIR"], built, spec["bin"])}
    args = [a.replace("{library}", library()) for a in spec["args"]]
    times, held = {label: [] for label in programs}, {label: [] for label in programs}
    for turn in range(runs + 2):
        for label, program in programs.items():
            took, ran, used = memory.run([program, *args], cwd=ws, text=False)
            if ran.returncode:
                wrong.append(f"runtime: {name} {label} run {turn + 1} exited with {ran.returncode}")
            if turn >= 2:
                times[label].append(took)
                held[label].append(used["largest_mb"])
    # The programs are one process each (ripgrep's threads are its own): its peak is the kernel's.
    result = {label: dict(median_s=round(statistics.median(t), 4), min_s=round(min(t), 4), max_s=round(max(t), 4), runs=len(t),
                          peak_mb=round(statistics.median(held[label])), peak_max_mb=max(held[label]),
                          size_mb=round(os.path.getsize(programs[label]) / 1e6, 1))
              for label, t in times.items()}
    print("  runtime    " + "  ".join(f"{label} {r['median_s']:.3f}s {r['peak_mb']} MB" for label, r in result.items()), flush=True)
    return dict(command=[spec["bin"], *spec["args"]], **result)


def machine(name=None, toolchain=None):
    """What the numbers were measured on."""
    def first_line(*argv):
        try:
            ran = subprocess.run(argv, capture_output=True, text=True, env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN))
            return (ran.stdout or ran.stderr).strip().splitlines()[0]
        except (OSError, IndexError):
            return None

    def read(path, pattern):
        try:
            found = re.search(pattern, open(path).read(), re.M)
            return found[1].strip() if found else None
        except OSError:
            return None

    memory = read("/proc/meminfo", r"^MemTotal:\s+(\d+) kB")
    cpu = read("/proc/cpuinfo", r"^model name\s*:\s*(.*)$")
    if not cpu and platform.system() == "Linux":
        # An Arm core's /proc/cpuinfo has no model name; lscpu names it from the part number.
        try:
            found = re.search(r"^Model name:\s*(.*)$", subprocess.run(["lscpu"], capture_output=True, text=True).stdout, re.M)
            cpu = found[1].strip() if found else None
        except OSError:
            pass
    if platform.system() == "Darwin":
        cpu = first_line("sysctl", "-n", "machdep.cpu.brand_string")
        memory = str(int(first_line("sysctl", "-n", "hw.memsize") or 0) // 1024) or None
    # A run pinned to some of the cores (taskset) has those.
    cores = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
    return dict(
        # Named by whoever runs it (the workflow names the instance type).
        name=name,
        cpu=cpu, cores=cores, machine_cores=os.cpu_count(),
        # What else the machine was running: 1, 5 and 15 minute averages when the run started.
        load=", ".join(f"{v:.1f}" for v in os.getloadavg()),
        memory_gib=round(int(memory) / 1024 / 1024) if memory else None,
        os=platform.system(), arch=platform.machine(), kernel=platform.release(),
        rustc=(toolchain or blaze_cli.selected_toolchain()).rustc, cc=first_line("cc", "--version"), ld=first_line("ld", "--version"),
        llvm=first_line("llvm-config", "--version"),
        incremental=os.environ.get("CARGO_INCREMENTAL", "profile default (on for dev)"),
        wrapper=None, linker="the toolchain's default (cc)",
        # Native linker inputs are part of the machine description.
        library_dirs=sum(arg.startswith("-L") for arg in os.environ.get("NIX_LDFLAGS", "").split()) or None,
    )


def blaze_commit():
    """The checkout's commit; a caller measuring another tree supplies commit in Run."""
    ran = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True)
    return ran.stdout.strip() or None


def compare(baseline, result, threshold):
    """Stored Blaze regressions, including missing prior evidence. Cargo is never relabeled."""
    worse = [f"baseline: {problem}" for problem in benchschema.errors(baseline)]
    worse += benchschema.errors(result)
    if worse:
        return worse

    def metric(label, field, before, after, floor, unit):
        if before is None:
            return
        if not benchschema.number(before):
            worse.append(f"baseline {label}: {field} is not finite and nonnegative")
        elif not benchschema.number(after):
            worse.append(f"{label}: {field} not measured or invalid")
        elif after > before * (1 + threshold / 100) and after - before > floor:
            worse.append(f"{label}: {field} {before}{unit} -> {after}{unit} "
                         f"(+{100 * (after - before) / max(before, 1e-9):.0f}%)")

    current = benchschema.measured(result)
    for key, was in benchschema.measured(baseline).items():
        now = current.get(key)
        if now is None:
            worse.append(f"{key}: not measured")
            continue
        for label, old in was["phases"].items():
            new = now["phases"].get(label)
            if new is None:
                worse.append(f"{key} {label}: not measured")
                continue
            for field, floor, unit in (("wall_s", FLOOR_SECS, "s"), ("rss_mb", 50, " MB"), ("compiled", 10, " functions"),
                                       ("edit_s", FLOOR_SECS, "s"), ("back_s", FLOOR_SECS, "s"),
                                       ("instructions", 1_000_000, " instructions")):
                metric(f"{key} {label}", field, old.get(field), new.get(field), floor, unit)
            before, after = ((p.get("memory") or {}).get("blaze", {}).get("peak_mb") for p in (old, new))
            metric(f"{key} {label}", "Blaze peak memory", before, after, 100, " MB")
            for field in ("built", "built_still"):
                if old.get(field) == "daemon" and new.get(field) != "daemon":
                    worse.append(f"{key} {label}: {field} {new.get(field)}, was the daemon")
            for field in ("link", "back_link"):
                if old.get(field) == "spliced" and new.get(field) != "spliced":
                    worse.append(f"{key} {label}: {field} {new.get(field)}, was spliced")
        before, after = ((p.get("runtime") or {}).get("blaze", {}).get("median_s") for p in (was, now))
        metric(key, "Blaze runtime", before, after, FLOOR_SECS, "s")
    return worse


def options(argv, name, default=None):
    """The values given to every `--name VALUE` in `argv`, which loses them."""
    found = []
    while name in argv:
        at = argv.index(name)
        found.append(argv[at + 1])
        del argv[at:at + 2]
    return found or ([default] if default is not None else [])


def sha256(path):
    with open(path, "rb") as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def compiler_identity(program):
    selected = Path(program).resolve(strict=True)
    paths = [selected]
    if selected.name in ("blaze", "blaze.exe"):
        engine = selected.with_name("blaze-engine.exe" if selected.suffix == ".exe" else "blaze-engine")
        if not engine.is_file():
            raise Failed(f"{selected}: forwarding CLI has no adjacent {engine.name}")
        paths.append(engine.resolve(strict=True))
    binaries = {str(path): sha256(path) for path in paths}
    return dict(path=str(selected), sha256=binaries[str(selected)], binaries=binaries)


def source_tree():
    """The harness checkout's source metadata, separate from a supplied compiler commit."""
    def git(*args):
        ran = subprocess.run(["git", "-C", ROOT, *args], capture_output=True, timeout=30)
        if ran.returncode:
            raise Failed(f"source metadata git {' '.join(args)}: {ran.stderr.decode(errors='replace')[-1500:]}")
        return ran.stdout
    return dict(head=git("rev-parse", "HEAD").decode().strip(), tree=git("rev-parse", "HEAD^{tree}").decode().strip(),
                diff_sha256=hashlib.sha256(git("diff", "--binary", "HEAD")).hexdigest())


def save(out, result):
    with open(out, "w") as stream:
        json.dump(result, stream, indent=1, allow_nan=False)
        stream.write("\n")


def run(given):
    work, out = os.path.abspath(given.work), os.path.abspath(given.out)
    if os.path.exists(work) and os.listdir(work):
        raise Failed(f"work must be new or empty: {work}")
    identity = compiler_identity(given.blaze)
    blaze_cli.BLAZE, blaze_cli.SOURCES = identity["path"], os.path.abspath(given.sources)
    blaze_cli.host.cache_clear()
    toolchain = blaze_cli.selected_toolchain()
    names = list(given.projects) or (
        [] if given.stress else [n for n, spec in PROJECTS.items() if "prepare" not in spec and not spec.get("named")])
    os.makedirs(work, exist_ok=True)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    result = dict(schema=RESULT_SCHEMA, compiler=identity, compiler_after=None, source_tree=source_tree(),
                  toolchain=toolchain.name, toolchain_info=asdict(toolchain), repository_toolchain=TOOLCHAIN,
                  machine=platform.machine(), cores=os.cpu_count(), runs=given.runs,
                  cold_runs=given.cold_runs, commit=given.commit or blaze_commit(), run_url=given.run_url,
                  date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), host=machine(given.machine, toolchain),
                  backends={b: BACKENDS[b] for b in given.backends}, results={})
    for name in names:
        for backend in given.backends:
            print(f"{name} {backend} ({blaze_of(backend)}; work {work})", flush=True)
            try:
                measured = measure(name, PROJECTS[name], backend, given.runs, work, given.cold_runs, given.tests)
            except (Failed, subprocess.TimeoutExpired) as failed:
                phases, wrong, ours, meter = SO_FAR.get((name, backend), ({}, [], None, None))
                for cleanup in (lambda: meter.close() if meter else None, lambda: stop(ours) if ours else None):
                    try:
                        cleanup()
                    except Exception as error:
                        wrong.append(f"cleanup: {error}")
                print(f"  FAILED {failed}", flush=True)
                measured = dict(phases=phases, wrong=wrong, failed=str(failed), tag=PROJECTS[name]["tag"],
                                rev=PROJECTS[name]["rev"], cold_runs=given.cold_runs)
            result["results"][f"{name}/{backend}"] = measured
            for line in measured["wrong"]:
                print(f"  WRONG {line}", flush=True)
            save(out, result)
            if not given.keep and not measured.get("failed") and not measured["wrong"]:
                for entry in os.listdir(work):
                    if re.search(rf"(^|-){re.escape(name)}-{backend}(-|$)", entry):
                        shutil.rmtree(os.path.join(work, entry), ignore_errors=True)
    if given.stress:
        crates, gap = stress_crates()
        result["stress"] = {}
        if gap:
            result["stress_gap"] = gap
            print(gap, flush=True)
        for backend in given.backends:
            for crate in crates:
                print(f"stress {crate['name']} {backend}: {crate['what']}", flush=True)
                key = f"{crate['name']}/{backend}"
                try:
                    result["stress"][key] = measure_stress(crate, backend, given.runs, work)
                except (Failed, subprocess.TimeoutExpired) as failed:
                    print(f"  FAILED {failed}", flush=True)
                    result["stress"][key] = dict(phases={}, wrong=[], failed=str(failed), tag="stress", path=crate["path"],
                                                 what=crate["what"])
                save(out, result)
                if not given.keep and not result["stress"][key].get("failed"):
                    for entry in os.listdir(work):
                        if entry.endswith(f"stress-{crate['name']}-{backend}"):
                            shutil.rmtree(os.path.join(work, entry), ignore_errors=True)
    result["compiler_after"] = compiler_identity(given.blaze)
    save(out, result)
    wrong = benchschema.errors(result)
    print(f"wrote {out}; findings: {len(wrong)}")
    for finding in wrong:
        print(f"  FAILED {finding}")
    return 3 if wrong else 0


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    given = load(sys.argv[1])
    if isinstance(given, Compare):
        baseline, result = (json.load(open(path)) for path in (given.baseline, given.result))
        worse = compare(baseline, result, given.threshold)
        print("\n".join(worse) or f"nothing over {given.threshold:.0f}% worse than the stored Blaze baseline")
        return 1 if worse else 0
    return run(given)


if __name__ == "__main__":
    sys.path.insert(0, os.path.join(ROOT, "tools", "typed-input", "src"))
    raise SystemExit(main())
