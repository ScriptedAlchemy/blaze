# SCRIPTEDITS: build scripts rerun across edits in the whole build

Unit p05w of the distributed program, 2026-10-26, branch `dist/p05w`. I ran everything below; "I
ran" means that. Claims are labelled as in the other reports: **measured** is a number I took
here, **read** is from the code, **reported** is another report's.

Unit p05r (2026-10-26, same branch) replaced the finding below: the whole build compiles and runs
its build scripts itself, with no `cargo check` and no rustc; see [blaze runs the
scripts](#blaze-runs-the-scripts-unit-p05r). The sections before it are p05w's, as it left them.

## What was asked, and what the whole build is

[distributed.md](../distributed.md)'s P0-D wants tracedecay's edits built by blaze, not cargo, and
the CLI's build script rerun as cargo reruns it. PR #5 tried that beside cargo's own builds and
failed four verifications on ever new interactions with the state cargo keeps in `target/debug`
(reports `u8-verify`, `-reverify`, `-verify3`, `-verify4`); the coordinator parked it and moved P0.5
to the whole build (`RA_JIT_WHOLE_BUILD=1`), where a distributed worker has no such state.

**Finding (read, then measured): the whole build does not run scripts itself.** Its worker loads the
workspace with rust-analyzer's `run_build_scripts`, which is a `cargo check` in `target/rust-analyzer`
with ra-jit as `RUSTC_WRAPPER`: ra-jit compiles the scripts, and cargo runs them under its own
fingerprints. After each edit the daemon decides whether to ask for that run again (`inputs.rs`,
`Inputs::stale`), and when it does cargo decides what runs. `crates/blaze-build-script`, which runs
and reads scripts as cargo does, is not wired into any build; only its `Rerun` rules and a new
`output::reads` are now used, by that decision. So this unit makes the daemon's decision cargo's, as
far as 34 probes can show; it does not make a worker free of cargo, and no unit of the plan does. That
is [no-rustc.md](../no-rustc.md)'s step I, K and the 107 scripts of P0.6.

## The probes

`harness/scriptedits.py` holds 34 cases ([harness.md](../harness.md)). A case is a project and steps
(an edit, a variable, a clean, nothing). Two directories hold the same project: blaze's whole build
(`cargo ra-jit build`, one daemon for the steps of the case) builds one, and cargo 1.99.0 (`cargo
build`, its own target directory) builds the other. The oracle, cargo, is never part of blaze's
build. Each script appends its package's name to a log outside the package, which nothing watches;
after each step the harness compares

- the build's exit status,
- how many times each script ran in the step,
- the program's exit status, standard output and standard error (the program prints what the script
  generated: a count, a variable, a cfg, a file's contents, `OUT_DIR`'s state),

and says which step differs. A step where cargo does not do what the case expects is reported as the
case's, not blaze's. Blaze's build has to say `[ra-jit]`, or it was cargo's. `--selftest` runs the
comparison on eight made-up differences and one right step, and finds each.

Where: Linux aarch64 in Docker (`rust:1.99.0-bookworm`, 8 GB VM, a container capped at 6 GB, jobs 6;
macOS fails the whole build today, #255, unit P0.6). `RA_JIT_MEMORY_MAX=5G`, `RA_JIT_OWN_BASE_MEMORY=3G`,
`RA_JIT_SPARE=0`, `RUST_BACKTRACE=0`: the brief's 16G and 12G are above the VM (a deviation, below).

Every step of every probe is equal in the build's exit status, the scripts' run counts and the
program's exit status, standard output and standard error (**measured**: the suite ran three times
in a row at 32 probes and twice at 34, once of them with no pause between the steps
(`SCRIPTEDITS_TICK=0`), each run about 80 s, all on the same binary: no flake). The
steps are cargo's own run counts ("2" in `links` is the two scripts, `native` and `probe`, each once).
"Before" is the same harness on `dist/main`'s binary (`3434bc93`, built the same way): 20 of 34 probes
equal, 135 of 163 steps.

| probe | steps | script runs per step (cargo, and blaze: equal) | blaze = cargo at, now | before |
|---|---|---|---|---|
| `counter` | 4 | 1 1 1 0 | 4/4 | 4/4 |
| `watched` | 9 | 1 1 1 1 1 1 1 0 0 | 9/9 | 9/9 |
| `watched-spelling` | 5 | 1 1 1 1 0 | 5/5 | 5/5 |
| `writes-what-it-watches` | 4 | 1 1 1 1 | 4/4 | 1/4 |
| `writes-its-package` | 4 | 1 1 1 1 | 4/4 | 0/4 |
| `dir-gone` | 5 | 1 1 1 1 0 | 5/5 | 4/5 |
| `unwatched-env` | 4 | 1 0 1 0 | 4/4 | 2/4 |
| `missing` | 6 | 1 1 1 0 1 1 | 6/6 | 4/6 |
| `missing-env` | 4 | 1 1 1 1 | 4/4 | 1/4 |
| `two-programs` | 3 | 1 1 1 | 3/3 | 1/3 |
| `env` | 10 | 1 1 0 1 1 1 1 0 1 0 | 10/10 | 10/10 |
| `env-new` | 6 | 1 0 1 1 0 1 | 6/6 | 5/6 |
| `env-later` | 4 | 1 1 1 0 | 4/4 | 1/4 |
| `rustflags` | 5 | 1 1 0 1 0 | 5/5 | 5/5 |
| `config-env` | 4 | 1 1 1 0 | 4/4 | 4/4 |
| `manifest` | 4 | 1 1 0 0 | 4/4 | 4/4 |
| `script-appears` | 4 | 0 1 0 0 | 4/4 | 4/4 |
| `restart` | 5 | 1 0 1 0 0 | 5/5 | 5/5 |
| `restart-missing` | 3 | 1 1 1 | 3/3 | 2/3 |
| `release` | 5 | 1 1 1 0 0 | 5/5 | 5/5 |
| `features` | 3 | 1 1 0 | 3/3 | 3/3 |
| `new-syntax` | 4 | 1 1 1 0 | 4/4 | 4/4 |
| `no-directives` | 6 | 1 1 1 1 1 0 | 6/6 | 6/6 |
| `directives-dropped` | 5 | 1 1 1 0 1 | 5/5 | 2/5 |
| `fails` | 5 | 1 1 1 1 0 | 5/5 | 5/5 |
| `fails-once` | 5 | 1 1 1 1 0 | 5/5 | 5/5 |
| `cfg-env` | 5 | 1 1 1 0 0 | 5/5 | 5/5 |
| `utf8` | 3 | 1 1 1 | 3/3 | 3/3 |
| `links` | 5 | 2 2 2 1 0 | 5/5 | 5/5 |
| `script-module` | 4 | 1 1 1 0 | 4/4 | 3/4 |
| `build-dep` | 4 | 1 1 1 0 | 4/4 | 3/4 |
| `out-dir` | 4 | 1 1 1 0 | 4/4 | 4/4 |
| `include` | 4 | 1 1 0 0 | 4/4 | 4/4 |
| `store` | 8 | 1 1 1 1 1 1 1 0 | 8/8 | 7/8 |

Both binaries ran every probe through the daemon (`[ra-jit]`); none was cargo's build.
`out-dir` set an extended attribute on Docker's overlay file system (`xattr_set=true`) and read it
back through three reruns, a hard link and a symlink beside it.

## What was wrong, and what fixed it

The fixes, each with the probes that answer it and its tests; what the probes found on `dist/main` is the table's "before".

1. **A file `rerun-if-changed` names that is not there is a change every build** (cargo's
   `find_stale_file`: `StaleItem::MissingFile`). The daemon watched a stamp of the path, which is the
   same while it is missing, so the script never ran again: `missing`, `missing-env`. Fixed with
   `Rerun::stale_files` (already in `blaze-build-script`, unused): `inputs.rs` asks it of each build.
   Tests: `a_file_that_is_not_there_is_stale_every_time`,
   `a_script_is_asked_for_a_file_that_is_missing_or_newer_than_its_run`.
2. **What a script says it reads is read as cargo reads it** and re-read after every run, not only
   when the workspace is loaded again. The daemon's reader took lines that start `cargo:`,
   untrimmed, from a file that has to be UTF-8 as a whole, and learned a new directive or variable
   only from the next load: `env-new`, `env-later`, `directives-dropped`. Fixed with
   `output::reads`, which is `output::parse`'s line rules without a package's targets. Tests:
   `what_a_script_reads_is_what_parse_reads` (equal to `parse` on seven outputs, invalid UTF-8,
   CRLF and both syntaxes among them).
3. **A script that says other than it did owes a run** (verify4's "removes all rerun directives",
   P1): cargo's fingerprint of a run is made of what the run before it said, so the next build runs
   the script again, once. The daemon now has `Script::owes`, carried over a load. Probes:
   `directives-dropped`, `no-directives`. Tests: `a_script_that_says_other_than_it_did_owes_a_run`,
   `what_a_script_says_it_reads_is_its_rerun`.
4. **The scripts run with the variables of the process that asks** (verify4's "newly watched user
   environment falsely certified", P1). A worker's request had the variables of the daemon, started
   by whoever started it, with the client's over them, so a variable the client had unset was set,
   and one a script began to watch after an edit was not passed on. A request is now the client's
   alone (`Worker::ask`), `run_env` gives cargo the client's variables over the daemon's and unsets
   those of a client before it that this one lacks. Probes: `env-new`, `store`. Tests:
   `the_scripts_run_with_the_variables_of_the_process_that_asks`.
5. **The script's own modules, and the workspace packages it is built with, are watched** (cargo
   compiles a script again, and so runs it, when one changes): `script-module`, `build-dep`. The
   modules are read from the dep-info of the compile that is there. Test:
   `a_script_is_asked_for_the_modules_it_was_compiled_from`; the build dependencies are the probe's.
6. **A script cargo runs every build is run once for each build.** A build is two requests to the
   worker (the load's first, then the build's), and one for each program: with fix 1 alone an
   always-stale script ran 3 times in the cold build, the load's and one for each request, where
   cargo's is 1 (**measured**, an intermediate build); a build of two programs asks once more for
   each (**read**). The daemon numbers its builds, says which one a request is for (`RA_JIT_BUILD`,
   not a variable of the process) and starts a worker for one (`RA_JIT_LOAD_BUILD`). Probes:
   `missing`, `two-programs`. Tests: `a_script_cargo_runs_every_build_is_asked_for_once_for_each`,
   `the_build_of_a_request_is_not_a_variable`.
7. **A load takes the scripts the check ran** (`ready`): a check that found the workspace other ran
   the scripts, and the reload ran them again, which an always-stale script runs. With it disabled
   `missing-env` fails on the third step on (the script runs 2 per build, `count=3` where cargo's is
   `count=2`, **measured**). The workspace stays stale until it is loaded, as before, when the request
   waits. No unit test (it needs a loaded workspace): the probe is its test.
8. **A script that says nothing of what it reads runs again for its package's files, as cargo's
   `PathSource` lists them**: those written since the run began, and what the others were. The
   stamp of the package included `target`, which cargo leaves out and which a whole build, with no
   `CACHEDIR.TAG` of cargo's at its root, fills with the program it links: every build asked for the
   scripts again. `Cargo.lock` is out too. A file the script writes into its package is a change
   for the next build, as under cargo. Probes: `writes-its-package`, `no-directives`. Tests:
   `a_script_that_names_nothing_is_asked_for_any_file_of_its_package`,
   `a_script_that_writes_its_package_is_run_again_in_the_next_build`.
9. **The store's kept run is keyed by what `OUT_DIR` held before the run** (verify4's "kept runs can
   replace a required execution", P1; probe 9 of the brief). Not reachable in the whole build, so by
   unit test only: `a_run_is_kept_for_the_contents_of_the_directory_it_ran_in`. A replay still skips
   what the script does beside `OUT_DIR`, so a script with such effects is one cargo would run and
   the store would not.

## Tests

`cargo +1.99.0 test -p blaze-build-script -p ra-jit`, Linux aarch64 in Docker: before 33 + 208 = 241
passed, after 36 + 217 = 253 passed, 0 failed. On macOS arm64 (the Mac, Rust 1.99.0) after: 36 + 217 passed, 0 failed.

## What I did not do

- **Run scripts in blaze.** See the finding above. A worker of a whole build still runs `cargo check`
  for scripts and proc macros, and rustc compiles some of what they need (`own_macro` and
  `own_script` take the rest). The Phase 0 plan's no-rustc boundary (P0-A) cannot hold on tracedecay
  until that is blaze's; the rules (`Rerun`, `reads`) and the probes are what a blaze runner is held to.
- **Make the daemon ask less.** It asks cargo in more cases than before, all those cargo reruns for:
  a script that names a file that is not there runs every build, as under cargo.
- **The cargo-hybrid mode** (`cargo ra-jit build` without the setting). `reruns_the_same` and PR #5's
  rest are untouched.
- **macOS, Windows.** The probes ran on Linux only. The code change is not platform specific; the
  tests that need Unix are `cfg(unix)`.
- **A script's effects outside `OUT_DIR` under the store's replay.** The key now holds what `OUT_DIR`
  had before the run, so a script that counts there is run again. A replayed run still does not do
  what the script does beside `OUT_DIR` (a log line, a file elsewhere). The whole build never
  replays (its scripts are not run through `ra-jit build-script`, **measured**: no `ra-jit-ran.json`),
  so no probe reaches it; the key is a unit test.
- **tracedecay.** P0-D's 30 of 30 needs P0.6 first.

## Deviations from the brief

- The brief says blaze is the only script runner in the whole build. It is not (the finding above);
  I fixed the daemon's decision, and did not start on a runner, which is the larger unit.
- Memory: the memory guard's 16G and 12G are above the Docker VM's 8 GB. The container is capped at
  6 GB with no swap (the kernel's limit is the watchdog) and blaze runs with 5G and 3G. The Mac's
  guard did not watch the container; the Mac ran only unit tests, under the heavy lock.
- `P0.5`'s old verify line named `harness/scripts.py --impure`, which does not exist; the impure
  marking it meant is C4's (distributed.md), and the line is now `scriptedits.py` and P0-D.
- The probes are 34, not nine: the brief's nine are several probes each (the verifier's cases were
  split by what they differ in), and I added those the reports hint at (a script that writes what it
  watches, a module of the script, a build dependency, a link, the manifest, RUSTFLAGS, `[env]`).

## Follow-ups

1. A runner in blaze: `blaze-build-script`'s `run` and `env` behind the whole build, its `Unit` filled
   from blaze's plan, the scripts compiled by blaze, the proc macros too. Then `Inputs` holds records
   of its own, which `Script` (`said`, `began`, `owes`, `asked`) already is, and the probes are its
   gate unchanged.
2. The cargo-hybrid mode keeps `reruns_the_same` (a script run beside cargo's, compared by bytes). It
   goes with PR #5, or when the mode does.
3. Windows and macOS runs of `scriptedits.py`, after P0.6.
4. **A whole-build bug the probes found, with no script in it** (**measured** on `dist/main`'s binary
   and on this branch): after `cargo ra-jit build --features x`, `cargo ra-jit build` again leaves
   `target/debug/<program>` built with the feature, where cargo rebuilds it without. Each feature
   set has a worker of its own and one program path; the worker of the first set answers
   "unchanged" and places nothing. `scriptedits.py`'s `features` probe stops before that step.

## Commands

```
git fetch origin && git switch -c dist/p05w origin/dist/main
docker run -d --name p05w --memory 6g --memory-swap 6g --cpus 10 -v $PWD:/work \
  -v p05w-target:/target -v p05w-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target \
  rust:1.99.0-bookworm sleep infinity
docker exec p05w rustup component add rust-src
# each of these through /Users/bytedance/dev/blaze-targets/with-heavy-lock.sh p05w, in the container:
cargo +1.99.0 build -p ra-jit --release                           # 2m26s cold, 22 to 38 s after
cargo +1.99.0 test -p blaze-build-script -p ra-jit                # 36 + 217 passed
RA_JIT_BIN=/target/release/ra-jit RA_JIT_MEMORY_MAX=5G RA_JIT_OWN_BASE_MEMORY=3G RA_JIT_SPARE=0 \
  RUST_BACKTRACE=0 PATH=/target/release:$PATH python3 -I harness/scriptedits.py --json OUT   # 34 of 34
python3 harness/scriptedits.py --selftest                         # on the Mac too
cargo +1.99.0 test -p blaze-build-script -p ra-jit                # macOS arm64: 36 + 217 passed
```
The base is `git archive origin/dist/main` built into `/target-base`; the "before" column is
`BIN=/target-base/release`. The container is removed when the unit is done.

## blaze runs the scripts (unit p05r)

Unit p05r, 2026-10-26, on `dist/p05w` (PR #12), after p05w-verify failed it on six findings. Same
labels as above. I ran everything here.

### What the whole build does now

**Read.** The whole build's load (`RA_JIT_WHOLE_BUILD=1`) no longer calls rust-analyzer's
`run_build_scripts`. `crates/ra-jit/src/scripts.rs` takes every package of the build that has a
script, in dependency order:

1. It describes the script and its build dependencies (their normal dependencies too, each with its
   features, its host profile from blaze-manifest and, when it has a script, what that said) as a
   `rust-project.json`, from blaze's own reading of the workspace, and has blaze compile and link
   it (`own_script::link_planned`: every crate from source over the standard library's unit,
   `--dep-info` and a new `--inputs`, the dep-info of every crate linked but the standard
   library's, with the variables `env!` and `option_env!` read). The compile runs with the
   variables of the process that asked, and `[env]`.
2. It runs the script with `blaze-build-script` (`env`, `run`, `Ran::finish`) in an `OUT_DIR` below
   `target/ra-jit/build-scripts/<profile>/build/<package>-<hash>`, the hash of the package, its
   features, profile, flags, target and compiler, as cargo keys a run's directory.
3. It reads what the script printed as cargo does (`output::parse`) and gives it to the crates the
   load reads, as `cargo check`'s messages would have (`OUT_DIR`, cfgs, variables); the link reads
   the libraries it named from the `output` beside `OUT_DIR`, as before.

When a script is compiled again, or run again, is cargo 1.99's rule ported into blaze
(`blaze_build_script::fresh` and `files`, from `fingerprint/mod.rs` and `sources/path.rs` at
5f94df478), over records of blaze's own beside what it made, not cargo's:

- **Compile** (`blaze-compile.json`): again when the description, the flags or the compiler
  changed, a file the `--inputs` dep-info names was written after the compile began, or a variable
  it read is other now (finding 2).
- **Run** (`blaze-run.json`, cargo's `Fingerprint` of a run): what the run depends on (the hash of
  the compile, the runs of the dependencies with `links`), the flags, the compiler and what the run
  reads (`Local`): the files and variables the last run named, or, when it named neither, the
  package's newest file as cargo's `PathSource::list_files` lists it: from git, tracked files and
  untracked ones git does not ignore, a tracked file in `target` included and no untracked
  `Cargo.lock`; or walked, without hidden files; `include` and `exclude` as gitignore lines;
  packages inside left out; nothing left out for a `CACHEDIR.TAG` (finding 3). The run is done
  again when that fingerprint is not the last run's, the output of the last run is not there
  (finding 4), the script or a `links` dependency was made after it, or a file it named is not
  there or was written after it began. A run that ends writes the fingerprint of what it named, or
  keeps what was read before it when it named nothing; a run that fails leaves no record.
- **A build judges each script once** (finding 1): the daemon numbers each `Request::Build` and
  the number goes with the request to the worker (`Worker::ask(.., build)`); the worker keeps
  nothing of the last build asked. The worker remembers which build judged each run directory, so
  the build's other requests (the load's and one per program) neither run it again nor, when it
  failed, succeed. A new worker's load runs with the variables of the client that started it
  (`RA_JIT_LOAD_ENV`), not the daemon's.
- **No replay** (finding 5): a run is never replayed from the store, in the whole build or by
  `ra-jit build-script`, whose kept runs (`Kept`, `run_key`, `out_dir_state`) are gone: what a
  script reads besides the files it names and what it does beside its `OUT_DIR` are in no key.
- **No rustc and no compiling cargo** (finding 6): the load's plan is blaze's too
  (`plan::Plan::own`: the packages and features of blaze's own metadata, blaze-manifest's
  `Profiles::get_profile`, the flags from the variables and the configuration), so the worker runs
  no `cargo build --unit-graph` and no `cargo config get`.
- **Failure** (requirement 5): a script that fails fails the build (status 101) with cargo's
  message (`Ran::finish`), and leaves no record, so the next build runs it again; nothing is
  retried within a build.

Not done: a build whose graph has a proc macro still has cargo's check build it, and that check runs
the scripts as before (the worker says so in its log); blaze building proc macros without the check
is no-rustc step J. A build dependency that is a proc macro is refused with the reason.

### The probes

`harness/scriptedits.py` has 42 cases: p05w's 34 and 8 for the six findings (finding 3 is three).
**Measured**, Linux aarch64 in Docker (`rust:1.99.0-bookworm`, container capped at 6 GB with no swap,
`--cpus 10`, `RA_JIT_MEMORY_MAX=5G`, `RA_JIT_OWN_BASE_MEMORY=3G`, `RA_JIT_SPARE=0`,
`RUST_BACKTRACE=0`), one release binary, two full runs in a row: the first 41 of 42 cases and 192 of
194 steps equal, the two others `no-compiler`'s, which then failed on any cargo run; the harness now
fails it on rustc, rustdoc and compiling cargo runs and shows cargo's queries apart, and the second
run is **194 of 194 steps, 42 of 42 cases equal** (`every case is cargo's`). Each run took about 3
minutes (from the files' times; not a benchmark). The steps column is cargo's runs per step, which blaze's equal.

| probe | steps | script runs per step (cargo's; blaze's equal) | equal |
|---|---|---|---|
| `counter` | 4 | 1 1 1 0 | 4/4 |
| `watched` | 9 | 1 1 1 1 1 1 1 0 0 | 9/9 |
| `watched-spelling` | 5 | 1 1 1 1 0 | 5/5 |
| `writes-what-it-watches` | 4 | 1 1 1 1 | 4/4 |
| `writes-its-package` | 4 | 1 1 1 1 | 4/4 |
| `dir-gone` | 5 | 1 1 1 1 0 | 5/5 |
| `unwatched-env` | 4 | 1 0 1 0 | 4/4 |
| `missing` | 6 | 1 1 1 0 1 1 | 6/6 |
| `missing-env` | 4 | 1 1 1 1 | 4/4 |
| `two-programs` | 3 | 1 1 1 | 3/3 |
| `env` | 10 | 1 1 0 1 1 1 1 0 1 0 | 10/10 |
| `env-new` | 6 | 1 0 1 1 0 1 | 6/6 |
| `env-later` | 4 | 1 1 1 0 | 4/4 |
| `rustflags` | 5 | 1 1 0 1 0 | 5/5 |
| `config-env` | 4 | 1 1 1 0 | 4/4 |
| `manifest` | 4 | 1 1 0 0 | 4/4 |
| `script-appears` | 4 | 0 1 0 0 | 4/4 |
| `restart` | 5 | 1 0 1 0 0 | 5/5 |
| `restart-missing` | 3 | 1 1 1 | 3/3 |
| `release` | 5 | 1 1 1 0 0 | 5/5 |
| `features` | 3 | 1 1 0 | 3/3 |
| `new-syntax` | 4 | 1 1 1 0 | 4/4 |
| `no-directives` | 6 | 1 1 1 1 1 0 | 6/6 |
| `directives-dropped` | 5 | 1 1 1 0 1 | 5/5 |
| `fails` | 5 | 1 1 1 1 0 | 5/5 |
| `fails-once` | 5 | 1 1 1 1 0 | 5/5 |
| `cfg-env` | 5 | 1 1 1 0 0 | 5/5 |
| `utf8` | 3 | 1 1 1 | 3/3 |
| `links` | 5 | 2 2 2 1 0 | 5/5 |
| `script-module` | 4 | 1 1 1 0 | 4/4 |
| `build-dep` | 4 | 1 1 1 0 | 4/4 |
| `out-dir` | 4 | 1 1 1 0 | 4/4 |
| `include` | 4 | 1 1 0 0 | 4/4 |
| `store` | 8 | 1 1 1 1 1 1 1 0 | 8/8 |
| `two-clients` | 3 | 1 2 2 | 3/3 |
| `compiler-env` | 6 | 1 1 0 1 1 0 | 6/6 |
| `package-cachedir` | 3 | 1 1 0 | 3/3 |
| `package-include` | 4 | 1 0 1 0 | 4/4 |
| `package-git` | 5 | 1 1 0 1 0 | 5/5 |
| `output-gone` | 3 | 1 1 0 | 3/3 |
| `out-dir-xattr` | 3 | 1 1 0 | 3/3 |
| `no-compiler` | 4 | 1 1 1 0 | 4/4 |

What `no-compiler` (`strace -f` of blaze's build: every program the client, the daemon and its
workers started) saw, **measured**: no rustc and no rustdoc in any step, and no `cargo check`; the
cold build asked cargo 8 times (16 execs, each through rustup's proxy) and the edit of the build
dependency 2, compiling nothing (below).

### P0-A on a hello-world with a build script and a build dependency

`harness/tracedecay.py --predicate P0-A --whole` (new: the whole build's P0-A, the build and the
program, `--whole-edit` for edits) with `norustc.py`'s stand-in rustc, rustdoc and cargo, on a
workspace of `helper` (a library) and `hello` (a program whose `build.rs` uses `helper` as a build
dependency and writes what `include!` reads), in the same container. **Measured**, one run:

| step | build | compiler runs | cargo runs |
|---|---|---|---|
| cold build | 0, 14.7 s | 0 | 8 |
| edit `helper/src/lib.rs` | 0, 0.6 s | 0 | 2 |
| edit `hello/src/main.rs` | 0, 0.04 s | 0 | 0 |
| build of nothing | 0, 0.03 s | 0 | 0 |

The program printed `hello helper=two edit=2`. **0 compiler execs** (p05w-verify measured 4 rustc
compiles and 2 `cargo check`s for the same shape). **P0-A's 0-cargo bar is not met**: the 10 cargo
runs are rust-analyzer's own loading, not the scripts': in the worker's load `cargo -Z
unstable-options config get` (`CargoConfigFile::load`), `cargo --version` (`version::get`), `cargo
rustc --print cfg` and `--print target-spec-json` (`QueryConfig::Cargo`), and `cargo metadata
--no-deps` and `cargo metadata` of the standard library's workspace (`Sysroot::load_workspace`),
which every load asks, the load of each script's compile (`ra-jit <rust-project.json>`) included,
so the edit of `helper` asks it twice. They answer questions and compile nothing (the stand-in
rustc ran 0 times). Replacing them is the next unit: the configuration and the standard library's
graph from blaze-manifest and blaze-metadata, as the workspace's already are.

### Tests

`cargo +1.99.0 test -p blaze-build-script -p ra-jit` on the Mac (macOS arm64), through
`with-heavy-lock.sh` with a private `CARGO_TARGET_DIR` and the top-MEM watchdog: **47 + 217 passed,
0 failed** (p05w's 36 + 217: 11 new in `blaze-build-script`, `files` and `fresh`; in `ra-jit` 3 new,
the concurrency test of the daemon's builds and two of the runner's, and 3 gone with the store's
kept runs). Watchdog: 77 samples, peak top MEM 2142 MB, no swap growth. Not run on Linux.
`python3 harness/rustcsites.py`: 15 places start rustc, rustdoc or cargo, none new, as before
(`scripts.rs` starts only blaze itself, and `files.rs` git, which compiles no Rust).

### Deviations

- P0-A's 0-cargo bar is not met (above); 0 compiler execs is.
- A whole build with a proc macro in its graph keeps cargo's check, which runs its scripts.
- `no-compiler` fails on rustc, rustdoc and compiling cargo, not on cargo's queries, which it lists.
- Memory: 5G and 3G in a 6 GB container, as p05w did, under the Docker VM's 8 GB.
- The probes ran on Linux only; the Mac ran the unit tests.

### Commands

```
git fetch origin && git switch -c dist/p05r origin/dist/p05w
docker run -d --platform linux/arm64 --name p05r --memory 6g --memory-swap 6g --cpus 10 \
  -v $PWD:/work -v p05r-target:/target -v p05r-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=6 -w /work rust:1.99.0-bookworm sleep infinity
docker exec p05r bash -c 'rustup component add rust-src; apt-get install -y python3 strace attr git time'
# each of these under with-heavy-lock.sh p05r, in the container:
cargo +1.99.0 build -p ra-jit --release
RA_JIT_BIN=/target/release/ra-jit RA_JIT_MEMORY_MAX=5G RA_JIT_OWN_BASE_MEMORY=3G RA_JIT_SPARE=0 \
  RUST_BACKTRACE=0 PATH=/target/release:$PATH python3 -I harness/scriptedits.py --work /suite --json /suite.json
python3 -I harness/tracedecay.py --td /p0a/hello --work /p0a-work --blaze-bin /target/release \
  --package hello --program hello --memory-max 5G --own-base-memory 3G --predicate P0-A --whole \
  --whole-edit helper/src/lib.rs one two --whole-edit hello/src/main.rs "EDIT: u32 = 1" "EDIT: u32 = 2"
# on the Mac, under the lock and the watchdog:
CARGO_TARGET_DIR=<private> cargo +1.99.0 test -p blaze-build-script -p ra-jit
python3 harness/scriptedits.py --selftest; python3 harness/rustcsites.py
```

## Open-PR review, 2026-10-26

This review checked the current p05r code and its surrounding callers, then merged the current
`dist/main` into the branch. The documentation conflicts were reconciled with the owner's parity
scope and the whole-build runner described above. These are additional review results; the earlier
end-to-end measurements in this report were not repeated.

### Corrections

- **Compiler identity:** script compilation and run records now include the source identity of
  the blaze build (or its executable identity outside a source build) and the installed toolchain's
  identity. Different compiler builds with the same package version and sysroot path no longer
  share the old script records.
- **Flags and package inputs:** the runner uses blaze-manifest's Cargo flag precedence, including
  matching `target.'cfg(...)'.rustflags` tables. Inherited `include`/`exclude` rules are read for a
  workspace's root package and for an explicit `package.workspace` path. Git-listed filenames and
  checkout paths keep their Unix bytes instead of silently dropping non-UTF-8 inputs or selecting
  the non-Git file walk.
- **Shared records:** an OS file lock covers the script freshness decision, compilation, execution
  and record writes in a target directory. Its guard explicitly unlocks before closing its file:
  a concurrent fork's inherited open description must not extend the completed critical section.
- **Build dependencies:** a package's host and target script results are distinct. A library
  compiled into a build script takes its own script's host output, profile, flags, `TARGET` and
  target cfgs, including when that package is also a normal target dependency. Host cfgs come from
  the existing built-in toolchain answers, without a compiler invocation. Their construction is
  shared with the general cfg answer; SDK settings that do not affect the built-in host cfg do not
  invalidate this narrower lookup.

### Validation performed

The review ran on Linux with Rust 1.99.0, one Cargo build job, debug information disabled and
incremental compilation disabled. `RUST_BACKTRACE=0` was set for the harnesses. No Mac or tracedecay
runtime result is claimed here.

| Check | Result |
|---|---|
| `cargo +1.99.0 check --locked --tests -p ra-jit` | Passed in a completely fresh target directory, 5m 30s. Repeated after the final source commit: passed in 0.71s. This typechecks the test targets; it does not execute them. |
| `cargo +1.99.0 test --locked -p blaze-build-script` | 49 passed, 0 failed. Both new non-UTF-8 regressions failed against the earlier file-listing code and passed after the correction. |
| Isolated production `scripts.rs` component probe | 15 tests passed, including the compiler identity, flags, inherited rules and host/target dependency regressions. It used the real project-model, manifest and build-script crates. Compiler and daemon integration was stubbed, and the compiler-commit lookup returned no commit for the fixture toolchains. It did not compile a user build script or test the daemon as a whole. |
| Final production `scripts/lock.rs` module | 2 tests passed: contention and release with a duplicated open description. The latter failed before explicit unlock was added. |
| `python3 harness/scriptedits.py --selftest` | All 12 comparator checks passed. This is the comparator's synthetic self-test, not the 42-case end-to-end suite. |
| `cargo +1.99.0 fmt --all -- --check`, `git diff --check` | Passed, including formatting corrections to four blaze-wire files inherited from the base. |

The host/target regression uses a real manifest and a toolchain identity fixture with no compiler
executable. It checks the script environment, profile and separate `OUT_DIR` paths, then supplies
distinct host and target script outputs and verifies that a build dependency consumes the host's
cfgs and environment. Setting `SDKROOT` made the initial general-query implementation fail; the
dedicated cfg lookup passes the same regression.

PR #5 remains parked. Its separate review tightened reads of Cargo's script records to require a
held lock and added the same explicit-release guard. Its production `script.rs`/`records.rs`
component probe passed 49 tests with one measurement intentionally ignored; unused store and daemon
entry points were stubbed. The duplicated-description regression failed before the guard fix and
passed afterward. That evidence does not overturn the hybrid mode's earlier failed verifications.

### Remaining limits found by reading the code

Two of the three limits this review found were fixed afterwards, by [the build-scoped
leases](#what-a-build-holds-of-a-script-leases); the third stands.

- ~~`scripts::JUDGED` remembers only the last build for each run directory.~~ Fixed: what a build
  judged is kept by (build, run directory) until the build is retired.
- ~~The target-directory lock ends when `scripts::run` returns, so another build can replace the
  generated files before the first build's compile and link read them.~~ Fixed: a lease on the
  run directory lasts until the build is retired.
- A graph containing a proc macro still takes the existing Cargo check path, which runs its
  scripts too; the direct runner also refuses a build dependency that is a proc macro. These
  remain the no-rustc step J gap described above. This review added no compiler fallback.

The review's tests establish the corrections listed here. They do not establish the whole of P0-A
or P0-D, nor replace the 42-case runtime suite or the platform checks required for landing.

## What a build holds of a script (leases)

Review `5661429749` of `91d58995` found two request-lifetime faults in the runner above; independent
reviews of `ac24007a`, `04d69ce7` and `ae31fc5c` found twelve more, the last four in the design this
section described before (a world of tokens, and a build that yields forgets what it judged). This is the
design that answers all of them, what ran of it, and what it does not do. **Whether they are fixed is for
the review to say**; each claim is marked **read** (from the code), **measured** (a run, with its source)
or **not run**.

### The lifecycle (read)

- A **build** is one `Request::Build` to the daemon (`next_build`), several requests to the worker
  (`RA_JIT_BUILD=<n>`, one per program). A script is judged by the worker's main loop in
  `Checkouts::stale` (every loaded checkout, one `run` each), in `reload`, in `join` and in the load
  (`add`: `--also`), and it reads in every mode there is to build in: the background thread,
  `RA_JIT_OVERLAY=1`, a checkout read as another, a link detached from the request; and not only for
  requests that named a build (a direct `ra-jit --exe`, an editor's `edit`, a rebuild nobody asked for).
- `OUT_DIR` is persistent and its path is observable. A copy at another path is not the output the script
  described, and replaying a script's other effects is what #5 did and was parked for. So the output is
  protected where it is. A script may have effects beside `OUT_DIR` that nothing undoes: a script cargo
  runs once in a build runs once in it.

### The design (read)

Cargo holds the build directory's lock for the whole build. The runner holds, instead:

- the **admission**, the script directory's transaction lock (`.lock`), owned by a *build* and by an
  **operation** of it: a request's judging of every checkout (`stale`, `reload`, `join`) or a load's
  (`add`, all its checkouts) has it from the first script to the last, however many `run`s that is, and
  keeps it across the times it is put off. No build is between the groups of another;
- a **lease** on each script's run directory (`flock` on `blaze-lease`), shared by the builds that read the
  output and exclusive for the one that writes it;
- a **decision** for each script a build judged, immutable and kept until the build is retired: a failure
  for good, and a result with the **generation** of the run it is of;
- a **world** for each loaded checkout: the run directories of its scripts, each with the generation of the
  run the database was loaded from.

A **generation** (`blaze-generation` beside `out/`, so not in `OUT_DIR`) is a number a run writes as it
begins, and a fresh judgment reads under the lease. It is not the fingerprint, which says what a run was
made from (a watched file by its path, not its contents or time): two runs of one fingerprint (a watched
file edited, a script that runs every build, one that is not deterministic) leave different output, and a
run that fails leaves what it likes. A world and a result are of a generation, and are the same as the
output only while the output has that generation.

The rules (`scripts/leases.rs`, `scripts.rs`, `main.rs`):

1. **A build that ran a script keeps the output and the admission until it is retired; a build that only
   found the output fresh yields.** A build that is refused what it needs lets go of the leases no consumer of
   its reads, keeps what it decided, and waits holding nothing; when it asks again a failure is its failure
   still and a result is its result if the output still has the generation it is of (`reacquire`). That is
   sound for a build that *found* a script fresh and ran nothing: if another build ran it meanwhile, judging
   it again is that build's first run of it. It is not sound for a build that *ran* the script: the output is
   the one run of it the build has, `OUT_DIR` is persistent and cannot be copied (a copy is not the path the
   script described, and replaying its other effects is what #5 did and was parked for), and a second run
   re-does what the script did beside it (an external marker, a counter, a generator that reads what it left).
   So the build that runs a script (`Writer::stands`) is an **executor**: its admission is let go by nothing
   but its retirement (`release_admission` is a no-op for it), so it never yields, no other build is
   admitted between its requests to wait for its output and force it to give the lease up, and a later
   request of the build, whichever group it judges, finds its own decision. Two executors cannot interleave
   (the second waits holding nothing), as cargo's build-directory lock has it for the whole build; builds
   that find their scripts fresh still share the output and the admission is theirs only for an operation.
   The cost is that a build that has to run a script excludes the other builds that have to judge one in
   that target directory until it is over: a build that has already decided its scripts reads on, but a build
   that arrives, even one that would find everything fresh, takes the admission before it judges anything and
   waits for the executor (`Runner::begin`); no throughput or latency was measured (the daemon retires a build at its answer, so that is one cargo
   invocation). A daemon build is one worker's, so the admission of another worker cannot be waited for by
   its own build. Not an exception: `reacquire` of a result the build *ran* whose output is not its run is an
   error and never a second run. The build that has the admission does not yield. With the admission an
   operation's, two loads that take R then S and S then R do not interleave, and yield is for a build that
   found a group fresh and asks for another after its operation (a later join).
2. **A consumer takes a `Pin` before its first read**: the consumer is counted first, so no release comes
   between, then shared leases on the world of the checkouts its program is built from (not every checkout
   loaded) and the generations looked at again. It lives with the client through the compile, the link, the
   errors and a link detached from the request, and not while the request waits to be built again (an
   interrupted build puts its clients back with no pin).
3. **A world that moved is loaded again, not refused.** The request is put back to be asked for as a request
   of the program would be, with its own variables: the check of the scripts judges them over the output as
   it is, and the checkout is loaded again if what they say is another. An editor's `edit` is put back with
   its text, and the checkouts are asked for as a `sync` of each program with the edit's variables; the edit
   is taken when they are loaded.
   **The edit waits for the recovery it queued** (`recovery.rs`): the requests that load the checkouts
   again carry a token, and the edit is not looked at again while any is alive, so a recovery that is put
   off (the script it runs is read by a build that is compiling) is waited for as long as that takes,
   by the worker's sleep, and the edit is neither counted nor asked about at every turn. A recovery that has
   ended and left the world moved again is another recovery when the output is another run's (another build
   ran the script since: that build's progress, and every one costs the editor a recovery and not its
   answer), and the end of the request, said, only when it is the same run's as the last (the recovery
   loaded the world and the output is still not what it was loaded from: asking again would ask for the
   same). There is no count of attempts. A `sync` or `patch` that comes through the check follows the same
   rule.
4. The load takes its owner from the request, tells the inputs what it watches only when the database has
   the checkouts, and looks at the generations of all of them before it does. A split that cannot be loaded
   again keeps its world. **A checkout that joins a running worker is loaded with the variables of the
   request that joins it** (`Checkouts::add`'s `env`), its first run of the scripts and its `Own`; the
   startup variables (`scripts::load_env`) are for the first load only. A build's first run of a joined group
   is then the world it asked for, which a `Standing` decision keeps for its later requests.
5. A request that must wait is put off (`Busy`) and asked again when `scripts::generation()` is not what it
   was when the request was **read, before the stale check**. A file of another process is watched by a
   thread for each **registration**: it ends its own and no other, under the lock registrations are made
   under, so a build that asks as the last one retires has a watch.

### Tests

Component (Mac, shared target, heavy lock, watchdog): `scripts::` and `recovery::` 38 passed at `4ae2a363`. They include decisions kept
across a yield (reacquire at the same generation; superseded when another build ran it; a failure kept), two
runs of one fingerprint as two generations (with a script that runs every build), a pin that is a consumer
before it has its leases (a pin in flight is not released under), an operation that has the admission
for all its groups, the watcher's retire / re-register / late-exit orders, one at a time (`register` and `ends` are two
functions under one lock; dropping the empty-set check fails the test, and so does dropping the "only my
entry" check; a threaded stress of the same window passes with either dropped, so it is not the gate), and the
earlier tests.

Linux through the daemon (`harness/leasebarrier.py`; Docker `rust:1.99.0-bookworm`, one 6 GiB container, no
extra swap, own docker lock). Baseline = `ae31fc5c` as published; fixed = production code `2c95ea1c`
(binary sha256 `c8f00791eda2…` in every batch):

| scenario | baseline `ae31fc5c` | fixed |
|---|---|---|
| `generation`: another worker runs the script again over a file it watches (one fingerprint), then an edit to A reads the generated file in a body | the edit prints `include=b env=a cfg=a` (the new payload with the old world); cargo prints `b b b` | `b b b`, as cargo's; the world was refused and loaded again (`refused` in the log), the edit taken |
| `unrelated`: S's world moved, an edit to R | refused: "not what the database was loaded from … ask again" | built |
| `groups`: two direct runs, `--also` in opposite orders, scripts that run every build, a guard that fails the second execution of one for one value | A fails: "script ran twice: gena-a" | each script ran once for each build (4 executions); both built; programs = cargo's |

Regression on the fixed build, same binary: `worlds`, `join`, `lost-wake`, `reload-busy`, `overlay`, `internal` pass.
`scripts::` 31 and `blaze-build-script` (unchanged) are as in the table above on the Mac; on Linux `scripts::` 31
passed at `2c95ea1c`. Evidence: `p05r-evidence/generation-recovery-2c95ea1c/` (a JSON, a folder and a manifest for
each batch, named by the batch and never overwritten).

### Build-once and waiting recovery (`0e580b5b` → `600411b9`; production `4ae2a363`)

The independent re-review of `0e580b5b` (Traex GPT-6.1-Sol) found two residual P1s, and gave the
diagnosis for both: a stale edit was counted at every turn of the worker while its recovery was put off
(four turns, then "ask again", while the build that held what it needed was compiling normally), and a
generation change re-ran a script in a build that had already run it (the test accepted `a, b, a`).
Both are corrected as rule 1 and rule 3 say. Linux through the daemon and a worker started by hand,
Docker `rust:1.99.0-bookworm`, one 6 GiB container, no extra swap, own docker lock; baseline = the
`0e580b5b` binary (`6514c23ab975…`), fixed = production `4ae2a363` (`917abb9ecd92…`), harness `600411b9`:

| scenario | baseline `0e580b5b` | fixed |
|---|---|---|
| `once`: ONE explicit build number (`RA_JIT_BUILD`) for A's first request (runs R, answered), a request that joins S, and a third; B, another build, asks for R with another `GEN` in between and is retired at its answer, as the daemon does; a guard fails the second execution of R for A | B is put off behind A's lease, then admitted when A yields for the join; B runs R; A's join and its third request fail ("failed to run custom build command … r": the script ran twice, the guard exits 7); the worker's log has `runs build=1001` twice for R | A runs R once; B is put off behind A's admission, and has run nothing while A's build lasts; A's join and third request are answered; B runs after A's retirement; A's program and B's are cargo's (native link aside) |
| `once-failed`: the same with a script that fails for A | the failure is A's for good; one execution; B goes on | the same (this is a control: the failure was already kept) |
| `recovery`: A's database is of `GEN=a`, B (`GEN=b`) is held after its scripts for 8 s, an edit to A needs the script run again for `a` | the edit is looked at five times in the first instants, "failed … ask again" at once | the edit is refused once and waits; B is let go, the edit is built, with no second request, and is cargo's |

Regression on the same fixed binary: `generation`, `groups`, `coab` (the opposite orders), `lost-wake`,
`unrelated`, `reload-busy` pass. Evidence: `p05r-evidence/build-once-waiting-recovery-600411b9/` (three
batches, each a JSON, a folder and a manifest). A first harness for `once` was wrong (it guarded a
joined checkout's script, which the worker re-judges for every build) and is kept as the first batch's
failure. The tested production `4ae2a363` and the published `d592dd49` differ only in comment lines of
`scripts/leases.rs`: the executable logic is unchanged, the files are not byte-identical. The scenarios were not run with the daemon's own `Request::Build`; `once` sends each
request itself with an explicit number, which is what the daemon does for the requests of one build.

### The recovery's end wakes the worker, and a joined checkout runs with its request's variables (`d592dd49` → this head)

The second independent review (of `d592dd49`) closed the two original P1s in their tested lifecycles and found
two more, both reproduced by the fixtures below.

- **The end of a recovery had no wake after it.** The token was an `Arc<()>`; a request that ends releases its
  owner first (`scripts::release`, a wake) and drops its token fields after. A detached link ends on a thread of
  its own and a success sends the loop nothing, so the loop could take the release's wake while the recovery was
  still under way, sleep, and the token be gone afterwards with no event: the editor waited for ever. The token is
  now a `recovery::Token`, whose last owner calls `scripts::wake` once the count of owners is already zero (a
  level: `Recovery::under_way` is false when the wake is called). It does not depend on how the request ends
  (an answer, an error, a cancel, an owner that is not its own). Component: the production token and a callback
  record `under_way` at the wake (once, and false); removing the wake fails it. The Sync/Patch recovery does not
  take a token (`Recovery::advances`).
- **A checkout that joins a running worker ran its first scripts with the startup variables.** `Checkouts::add`
  took `scripts::load_env()` whatever the owner: A's request `GEN=a` joined S and S's first run was `GEN=load`
  (`include=load env=load cfg=other native=108`, where cargo says `a a a 97`), and A's `Standing` decision kept
  it. The request's variables are passed through `join`, `add` and `add_once`; the first load keeps the startup's.
  That "observed, not changed" was the next finding (below).

| scenario (Linux, hand-started worker, Docker `rust:1.99.0-bookworm`) | baseline `d592dd49` (+ the `released` barrier hook, nothing else) | fixed |
|---|---|---|
| `recovery-detached`: a checkout V read as S (`RA_JIT_OVERLAY=1`), so every build is detached; B changes the file R's script watches; an edit to R; the recovery request is held after its owner is released and before its token goes (one worker pid, one request) | the edit is never answered (one `barrier-released`, then nothing, `LEASEBARRIER_LOST=60`) | the edit is answered ok, no other request |
| `once` (A's `GEN=a` request, then the join of S): A-S is **executed** | `gen-one include=load env=load cfg=other native=108`; S's trail is `load load` | `include=a env=a cfg=a native=97` = cargo's; S's trail starts `a A` |

Regression on the fixed binary in the same batches: `once`, `once-failed`, `recovery`, `join`, `worlds`,
`unrelated`, `groups` pass. A first attempt of `recovery-detached` ran the recovery as `ok unchanged` on the main loop and
could not reach the detached link; it is kept, not counted. Evidence: further batches in
`p05r-evidence/build-once-waiting-recovery-600411b9/`, with the binaries.

### A request judges the checkouts it reads (`6bcfea42` → this head)

The third independent review found the rest of that observation to be a defect. The check of a request
visited every full checkout; those the request does not read were judged with the variables they were last
asked with, under the request's build number, and a script that runs executed then and its result stood
as that build's: B's request for R ran S's script with A's `GEN=a`, and B's explicit later request for S with
`GEN=b` found a `Standing` decision of `a` (no freshness look at all) and consumed `include=a`.
`Checkouts::stale` now judges (and loads again) only the checkout the request reads and the one it shares
packages with (`reached`; for a project that is not loaded, see the next section). Another checkout is judged for the build that asks for it, with that request's variables, as its
first judgment, once. Nothing is run a second time to correct the first, and no decision is keyed by
variables. What is given up: a loaded checkout nobody asks for is not noticed to be stale (its watched files,
its scripts) until someone does; it is not read by anyone else's program, and its world is only pinned by
programs built from it. The initial load, the join's first run and the recovery's variables are unchanged.

| `once`, B's build asks for R (`GEN=b`) and then, before it is retired, for S (`GEN=b`); S's script runs every build | baseline `6bcfea42` | fixed |
|---|---|---|
| S's trail after B's request for R | `a A`, `a A` (S ran for B with A's variables) | `a A` (B's request for R ran nothing of S) |
| B-S executed | `include=a env=a cfg=a native=97` (cargo: `b b b 98`) | `include=b env=b cfg=b native=98` = cargo's |
| S's runs | A once, B once (the wrong variables) | A once (`a`), B once (`b`) |

Regression on the fixed binary in the same batch: `once-failed`, `recovery`, `join`, `worlds`, `unrelated`,
`groups` pass; the detached recovery witness was not repeated (the change does not touch it).
Component: `reached` for a project that is not loaded, its own checkout, and a split's base. Mac `scripts::`,
`recovery::` and `reached` 40 passed.

### A project that is not loaded judges the checkout it will share packages with (`c0a64de1` → this head)

The fourth independent review found that the first version of `reached` kept one case wide: for a project
that is not loaded yet, every loaded checkout was judged, before it was known whether the project would be a
split of one or would load on its own. A loaded R whose script is valid for `GEN=a` and fails for `b`, and a
request for an independent S with `GEN=b`, failed with R's script before S was reached; R could also have had
effects of its own. `Checkouts::stale` now asks `alike(project)` (it compares files, and runs nothing) for the
checkout the project will share packages with, and judges that one with the request's variables; a project
that will load on its own reads no loaded checkout, and its scripts are judged when it loads, with its owner
and its variables. A split that could not be loaded again is asked to be by the requests that read it or its base,
and by no other. Not changed: the initial load, the first join's variables, the recovery's variables, the
executor's admission, a loaded project's own request (`asked`). A view the daemon makes (`view_of`) needs
`builds_alike`, as `alike` does, so the two agree; `RA_JIT_SPLIT=0` makes a project load whole although
`alike` finds a base, and then that base is judged needlessly (as it always was). That was the next
finding, below.

| Linux, hand-started worker, R loaded with `GEN=a` (valid for `a`, fails for `b`, keeps a trail) | baseline `c0a64de1` | fixed |
|---|---|---|
| `first-join`: ONE first request for S (another package and script, no relation to R), `GEN=b`, build 3001 | fails with R's message (`gena … (…/r)`); R's trail `a load, a serve, b B` | S built (`include=b env=b cfg=b`; `native=97` where cargo has 98, recorded as the native-link gap); R's trail unchanged |
| `first-join-shared`: V, a checkout of R's repository with R's scripts (read as R) | R judged once for `b` (trail `b B`), object built | the same: the checkout it shares is still judged |

`once`, `once-failed` (its join now says S's own failure, and R's is said again for R), `recovery`, `join`,
`worlds`, `unrelated` and `groups` pass on the fixed binary (the batch before the last harness edit, same
binary). Component: `reached` has no arm for an unknown project (40 passed with the other `scripts::` and
`recovery::` tests). The program V prints is recorded, not asserted: the viewed request builds an object and
the worker's executable is R's.

### The same policy as the consumption (`da9c551b` → this head)

The fifth independent review found that `alike` alone is not the policy: `add_once` shares only when
`RA_JIT_SPLIT` is not `0`, and `view_of` reads a checkout as a loaded one only when overlays are on (always,
or quick). With both off a file-alike V loads on its own, but the check still judged R: R's script, legal for
V and failing in R (a script may read its manifest directory, or a watched file), failed the request before V
was reached. `Checkouts::sharing` now takes the decision the consumption takes, before any script is judged:
`viewing(overlay, quick)` (the test `view_of` applies, which now uses the same `alike`) or `splitting()` (the
test `add_once` applies). `quick` is built before the check as it is after (`Quick::of`). Not changed: the
join's variables, the loaded project's own request, the base a sharing project reads (judged with the request's
variables), the executor's admission.

| Linux, `RA_JIT_SPLIT=0`, `RA_JIT_OVERLAY=0`, R loaded with `GEN=a`, V file-alike with the same bytes; R's script fails for `b` in R's directory only | baseline `da9c551b` | fixed |
|---|---|---|
| `first-join-private`: ONE first request for V, `GEN=b`, build 3001 | fails with R's message (`gen … /r`); R's trail `a load, a serve, b B`; V's trail empty | V built: `include=b env=b cfg=b native=98` = cargo's; R's trail unchanged; V's own script ran once, for `b` |
| `first-join-shared` (default policies) | R judged once for `b`, object built | the same: the base it reads is still judged |

Regression on the fixed binary: `first-join`, `first-join-shared`, `join`, `worlds`, `unrelated`, `once`,
`once-failed` pass (second batch, same source). Component: the policy matrix on the production `viewing` and
`sharing_allowed` (overlay never / quick / always, quick or not, split on or off), with `reached`; 41 passed with the other
`scripts::` and `recovery::` tests. Not shown: several file-alike candidates, mixed placed/split roots, a file
changed between the choice and the load.

### One choice of the checkout a project is read through (`b09055d3` → this head)

The sixth independent review found the same invariant broken in time: the checkout an unloaded project
is read through was decided twice, by the quick test of two moments. Before the check (which judges its scripts) a
base that is relinking in the background is not quick, so with `RA_JIT_SPLIT=0` nothing was judged; the
worker then settles (it waits for the link, and turns to the request's variables), the link ends, the base is
quick, and `view_of` took the view. V was read through a base whose scripts were not run for V's variables
(the program had the base's directives), and the pin of that base's `a` world succeeded.
`Checkouts::sharing` now decides once, before the check; its checkout is the one judged (`stale` takes it), and
the view taken later is accepted only if it is that one (`judged_view`; a loaded project's own view is
never in question). Otherwise the project is loaded on its own, as the first choice said, which is legal. Not changed: the
policy matrix, the join's variables, the executor's admission. A test hook holds a build as it begins to link
(`linking`, inert without the barrier directory).

| Linux, `RA_JIT_SPLIT=0`, quick overlay; R loaded `GEN=a`; V file-alike; R's executable taken away so a request for R relinks in the background and is held as it enters the link; V's first request, `GEN=b`, waits for it; the link is let go | baseline `b8a011fe` (= `b09055d3` + the hook) | fixed |
|---|---|---|
| route | `view: …/v as …/r` (V read through R) | V loaded on its own (no view line) |
| V's script | never ran (trail empty), no V program | ran once, for `b` (trail `b B`) |
| R's script | `a load, a serve, a A` (R's own request) | the same, no `b` |
| V's program | none (`first.bin` is not written) | `include=b env=b cfg=b native=98` = cargo's |

`first-join-shared` and `first-join-private` pass on the fixed binary in the same batch. Component:
`judged_view` over loaded / unjudged / judged / other base; 42 passed with the other `scripts::`, `recovery::` and
policy tests. Not shown: the same with splitting on (the base is judged before and after by the same
rule), several candidates, a base that becomes quick after the check in other modes (Always never depends on it).
The container's HTTPS to crates.io was intercepted in this session, so the Linux build used the registry
crates of `Cargo.lock` as `cargo vendor --locked` wrote them on the Mac (mounted read-only) and the host's
`rust-src` component; the failed batches before that are kept.

### The choice is the request's, over the view and the join (`cc92886c` → this head)

The seventh independent review found the choice kept only as a filter on the view: `add_once`, the split
fallback of the join, chose `alike(V)` again, after the check had reloaded a checkout and so changed the loaded
metadata `builds_alike` compares. With R1 and R2 loaded in full and R1's manifest made R2's (V's) before V's request,
R1 was the checkout V was alike when the request was checked (its loaded manifest still selected `build.rs`, and
the unused `build.rs` was the same), so R1 was reloaded and its new script judged for `b`; R1's new script then
differs from V's, R2 became the checkout V is alike, the view of it was refused, and the join split V from R2,
whose `a` scripts were not judged: V's program had `a`'s directives.
The choice is now a typed state, `Sharing` (`Open` for a load that is not a request's join: the first, with `--also`, and
a loaded project; `Alone`; `From(root)`), made once by `Checkouts::sharing` before the check, and given to `stale`
(the checkout to judge), to `view_of` and to the join (`add`, `add_once`): `alike_by` finds only among the checkouts
it allows. A choice that no longer holds loads the project on its own, with the request's owner and variables, which
is legal. Nothing is run twice or keyed by variables; the first load is untouched.

| Linux, split on, quick overlay; R1 and R2 loaded in full under `GEN=a` (no `split` line); R1's manifest made R2's; ONE request for V, `GEN=b`, build 5001 | baseline `cc92886c` | fixed |
|---|---|---|
| route | `split …/v` (from R2) | `join …/v` on its own |
| trails gained | R1 `b B`, R2 none, V none | R1 `b B`, R2 none, V `b B` |
| V's program | `include=a env=a cfg=a native=97` (cargo: `b b b 98`) | `include=b env=b cfg=b native=98` = cargo's |
| control, no edit | V split from R2, R2 judged once for `b`, V `b b b 98` | the same |

`quick-choice`, `first-join`, `first-join-shared`, `first-join-private` and `join` pass on the fixed binary in the same
batch. Component: `Sharing::allows` / `judged` (open, alone, from one root; another root supplies nothing), with the
other `scripts::`, `recovery::`, policy and `reached` tests: 42 passed. The Linux build and run need a pinned setup that the
product does not do for itself: the registry crates of `Cargo.lock` from `cargo vendor --locked` (read-only), the host's
`rust-src` for the toolchain, in a 6 GiB container (no extra swap), under the docker lock. Not shown: several candidates,
mixed placed/split roots, root-changing reloads, intervening source edits.

### A candidate is certified by its files, not by metadata that may be old (`d2ffc096` → this head)

The eighth independent review found that the candidate was still chosen by comparing the current manifests
with the script targets of the workspace as it was loaded. Once R1's manifest was made V's, R1 was alike by its old
target (the same unused `build.rs`) while it selects another script now; the check reloaded R1 and ran that script for
V's request, and when it fails V failed with it, before the choice could be revalidated and V loaded on its own. `alike_by`
now finds a candidate only among the checkouts whose watched files (their manifests, lock, configuration, build scripts)
are as they were when they were loaded (`Inputs::describes`: stat only, nothing run, no metadata refresh). One that is not is
no checkout to read another through until a request for it loads it again; the next alike checkout, or none, supplies the
packages. The cost is a worktree that loads on its own, instead of sharing, while its twin has a changed manifest. Typed
`Sharing`, the first load and a failing base V truly consumes are unchanged: the failure of a consumed base is said.

| Linux, split on, quick overlay; R1 and R2 loaded in full under `GEN=a`; R1's manifest made R2's/V's (`Cargo.toml` hashes equal after); ONE request for V, `GEN=b` | baseline `d2ffc096` | fixed |
|---|---|---|
| `choice-edited-failing`: R1's newly selected `build-new.rs` fails for `b` (in R1's directory only), V's succeeds | fails with R1's message (`gen … /r1`); R1's trail `b B`; no V program | V split from R2, judged once for `b` (trail `b B`); R1 not run; V `include=b env=b cfg=b native=98` = cargo's |
| `choice-edited` (R1's new script succeeds) | R1 judged (`b B`), V on its own (`b B`), `b b b 98` | R1 not run, V split from R2 (`b B`), `b b b 98` |
| `choice-stable` (no edit) | V split from R2, R2 `b B`, `b b b 98` | the same; the full tuple is now asserted (a mismatch fails, native alone is a recorded gap) |
| `choice-failing-base` (V's and R2's shared script fails for `b`) | fails with R2's message; R1 not run | the same: a consumed base's failure is said |

`quick-choice`, `first-join`, `first-join-shared`, `first-join-private` and `join` pass on the fixed binary in the same
batch. Component: `Inputs::describes` (not watched; as loaded; manifest selects another script; another root), 43 passed with the
other `scripts::`, `recovery::`, policy, `reached` and `Sharing` tests. Same pinned Linux setup as before (vendored
registry crates, host `rust-src`, 6 GiB, no extra swap). Not shown: several candidates beyond these three roots, mixed
placed/split roots, a candidate whose watched files change between the choice and the load, a checkout whose
files are touched without a content change (it is not shared until it is loaded again).

### The structure of a workspace is compared by content (`310d39f4` → this head)

The ninth independent review found that `Inputs::describes`, and `Inputs::stale` with it, took a manifest to be as
loaded when its size and time of last write matched. A selector replaced by one of the same size (`build-old.rs` →
`build-new.rs`, 12 bytes each) with the time put back kept the stamp: R1's loaded metadata still selected the old
script, `builds_alike` compared that old script (the same in all three checkouts), R1 was chosen, and its old
script, which fails for `b`, was run for a request whose own script is the new one.
The files a workspace's structure is read from are now `Structural`: each by its content, read again and compared
(bytes; absent; not a file; a file that cannot be read is never equal to anything, so it certifies nothing).
`describes` (the candidate) and `stale` (the selected checkout, which would otherwise compile from stale targets)
use it; a file written again with the same bytes is the same (a touch is not a change). At the record
(`Inputs::watch`, which is after the load) the script each package's manifest selects is compared by
file name with the loaded targets: a manifest written between the load and the record marks the checkout
inconsistent, to be loaded again. Package script freshness (cargo's rules over times) is unchanged.
Scope: only that one property (the selected build script) is bound to the loaded metadata; the other structural files are
compared with their content at the record, which is after the load, so a change to one of them between the load's
read and the record is not seen (the loader does not hand over the bytes it read); script names are compared by
file name, so the same name in another directory is not told apart. The cost is a read of the watched files (manifests,
lock, configuration) for the selected checkout at each request and for each candidate.

| Linux, split on, quick overlay; R1 `--also` R2 loaded in full under `GEN=a`; R1 selects `build-old.rs` (fails for `b`), R2 and V `build-new.rs`; R1's `Cargo.toml` replaced by R2's: size 173 → 173, mtime ns equal, hash `eb21bfa8…` → `b243d472…`; ONE request for V, `GEN=b` | baseline `310d39f4` | fixed |
|---|---|---|
| `choice-same-stamp` | fails with R1's message (`gen … /r1`); no V program | R1 not run; R2 judged once for `b`; V split from R2; `include=b env=b cfg=b native=98` = cargo's |

On the fixed binary in the same batch: `choice-edited-failing`, `choice-edited`, `choice-stable`, `choice-failing-base` (a consumed
base's failure is said), `quick-choice`, `first-join`, `first-join-shared`, `first-join-private`, `join`, `once`,
`unrelated`, `worlds`, `recovery`, `reload-busy`, `lost-wake` pass. Component: the same-size, time-restored replacement
(it fails if the content comparison is removed: mutation run), a file written again with the same bytes, restored,
appearing, removed, unreadable (never certifies; skipped when run as a privileged user), and the selector read from the
manifest. 52 passed with `inputs::`, `scripts::`, `recovery::`, policy, `reached` and `Sharing`. Same pinned Linux setup. The `native=97`
of `first-join` against cargo's 98 remains a recorded gap there; no gap in the choice witnesses.

### The selected script is compared by the file it is (`8c2e753b` → this head)

The tenth independent review found the registration's comparison of the script a manifest selects with the loaded target
reduced to the file name: `old/build.rs` and `new/build.rs` were one script. A script that rewrote its own package manifest
(a legal persistent effect of its first run) from `old/build.rs` to `new/build.rs` left a workspace parsed for the old
selector and a manifest, read at the record, that says the new one; the guard saw `build.rs` twice and certified it. A later request whose own script
is `new/build.rs` (V, with the same old script dormant) chose that checkout and ran its old target, which fails for `b`.
The identity is now the path normalized as cargo normalizes a target's (`.` and `..` by name) and then the file itself when it is there
(a link, a prefix), on both sides (`script_identity`, `selects_loaded_scripts`); the manifest is read once and the bytes recorded
and the selection compared are one state (`Structural::read`). A manifest of a shape this does not read says nothing. The
next request of a checkout so marked loads it again (the metadata then agrees); a candidate is never taken meanwhile.

| Linux, split on, quick overlay; R's manifest selects `old/build.rs` (succeeds for `a`, fails for `b`; its first run rewrites the selector to `new/build.rs`); V's manifest is R's after that (`eebeaa3e…` both), the same dormant `old/build.rs` (`4f338d8b…`/`2cf9143f…` per build), a `new/build.rs` that succeeds for `b`; V's request is the first after the load | baseline `8c2e753b` | fixed |
|---|---|---|
| `selector-moves`, ONE request for V, `GEN=b` | fails with R's message (`gen … /r`); R's obsolete old script was the one run; no V program | R not run; V loads on its own (`join`), its script ran once for `b`; `include=b env=b cfg=b native=98` = cargo's |

`choice-same-stamp` and `choice-stable` pass on the fixed binary in the same batch. A request after the load, before V's, reloads R on
the rewritten manifest (the fixed registration marks it), after which R is V's twin: the scenario therefore makes V's request the
first after the load. Component: the registration's identity (same name in two directories differs; `./` and `..` and a link to the directory
agree; none; a manifest not read), the manifest recorded from the bytes read, with the others: 54 passed; a mutant that compares only
the file name fails the identity test. Same pinned Linux setup. Capture limit unchanged: apart from the selected scripts, structural files are
bound to their content at the record, which is after the load.

### The flags for rustc of a script's compile, an explicit `--target`, and the whole failure message (`0958842` → this head)

Three blockers that were named, closed in small steps with cargo as the oracle.

- **Flags.** `host_flags` only keyed the script's compile: the owned compiler was given none of RUSTFLAGS' `--cfg`, so a script
  reading `cfg!(hostflag)` saw the wrong thing. `apply_flags` orders what changes a crate into its description (`--cfg x` and
  `--cfg=x`, `-C debug-assertions[=yes|no|on|off]`, joined or not; `--check-cfg` changes no program); every other flag is
  named on stderr (`build scripts: not applied to blaze's compile of …`) and not applied. Not claimed: that the compile honours `-C panic`,
  `overflow-checks`, `-L`/`-l` or the rest.
- **Explicit target.** `--target`/`CARGO_BUILD_TARGET` naming the host is cargo's cross build: the scripts and proc macros get none of
  the flags, the program's units do, artifacts are under `target/<triple>`, and the script runs with the target's flags in
  `CARGO_ENCODED_RUSTFLAGS`. `Asked.explicit_target` (from `Selection.target`) and `Runner.cross` carry it.
- **The whole failure message.** A reply is one line and the client read one; cargo's message of a failed script has several (the cause, the script's
  stdout and stderr). The worker escapes it onto one line (`failed_reply`), the daemon and the sync client read it back (`unfailed`).

| Linux, `RUSTFLAGS="--cfg hostflag"`, a script that prints `cfg!(hostflag)` and its `CARGO_ENCODED_RUSTFLAGS` | baseline `eed39779` | fixed |
|---|---|---|
| `hostflags`, no `--target` | `saw=no` (cargo: `saw=yes`) | `saw=yes flags=--cfg hostflag target-cfg=true` = cargo's |
| `hostflags`, `--target <host>` | `saw=no` (equal to cargo's only because no flag was applied at all) | `saw=no flags=--cfg hostflag target-cfg=true` = cargo's, in `target/<triple>/debug` |
| `once-failed`: the script prints `script failed for a` on stderr | the client gets `failed to run custom build command …` only | the cause and `script failed for a` arrive |

`first-join`, `join`, `once`, `choice-stable`, `first-join-private`, `reload-busy` and `lost-wake` pass on the fixed binary in their batches.
Component: `apply_flags` (cfg both forms, debug-assertions values, what is named and does not eat the next flag), the one-line failure round trip.

### The inputs of a link are its identity (adapted from the prepared `repair/pr12-native-inputs`)

A warm worker took a script's native output to be unchanged when its directives, the Rust crates and the sizes were: a library renamed
for a value (`link-strict`), or an archive of the same name and path whose bytes changed (`archive-strict`, `native-only`), was patched or
spliced and the program kept the old one, and the harness recorded it as a gap. Worse than `native=97` for 98: in `link-strict` the second build
printed `include=a env=a cfg=a` for `GEN=b` (the objects were built, the executable was not linked).
`native_inputs.rs` keeps, for each executable a whole build links, the identity of its last successful link: a digest of the link line
(the real `link::command`) and of the content of each file it can name, in each search directory (the scripts' `OUT_DIR`s, `-L`, `LIBRARY_PATH`),
absent ones too (an earlier candidate appearing changes the link), with symlinks and directories followed and cycles refused. A change, looked at
before the no-op paths of a request (`native_inputs::stale`, `invalidate`) and again as the program is built (`begin`), forgets the base and the splice and
links in full; the identity is published only by a link that succeeded (`commit`, not for a deferred or compile-only one) and whose inputs held while it ran,
and the old one is removed when a link begins, so A, B, a failed link, A is not taken for reuse. A closure that cannot be said (a response file, an unreadable
path, a crate not loaded) links in full and refuses nothing. The link line is regenerated from the current directives before every link. Credit and
provenance: the identity, the pending-then-commit and the tests are those of `.github/repairs/pr12/native_inputs.rs` of `repair/pr12-native-inputs`
(7fc32028), read through Git and never applied (its `apply.py` checks blobs of an old main, and that branch's CI failed before testing it); the collection no
longer fails the request, a never-linked program is not stale, and the call sites are this head's. Cost: the files named by a link are read at each request.

| Linux, one warm worker | baseline `9f5aac12` | fixed |
|---|---|---|
| `native-only`: only the archive's bytes change, 97, 98, 97; a corrupt archive; its restoration with size and time kept; a no-op | 98 build prints 97; the corrupt archive is accepted as an unchanged success | each build = cargo's, the corrupt archive fails, the restored one links, the no-op neither relinks nor runs the script |
| `link-strict`: a library named for the value, a, b, a | `GEN=b` prints `include=a env=a cfg=a native=97` | every build = cargo's |
| `archive-strict`: one name, contents a, c, a | `GEN=c` prints `native=97` (cargo 99) | every build = cargo's |

On the fixed binary: `once`, `join`, `first-join`, `choice-stable`, `hostflags`, `internal`, `generation`, `overlay`, `lost-wake`, `reload-busy` pass; `worlds` and
`groups` timed out at 120 s on a host at load 50 (a first-load of 190 s) and pass with the timeout at 400 s; `coab` timed out at 400 s and at 1200 s on that host (a nested compile of a build script held 15 cores for 17 minutes) and passes, as it did at
`4ae2a363` in the same session, when the host load was 8-15. These timeouts are infrastructure (the Mac was at load 40-55 from other work), kept as failed attempts. Component: the module's seven tests (search directory does not hash the link's outputs; an earlier candidate appearing; opaque response file; same size
and time, other bytes; deletion, recreation, directive-only; failed or changed links do not commit; symlinks and cycles).

### What is not shown, not fixed, or wider

- **A warm worker's native link (closed for the cases in "The inputs of a link are its identity")**; one gap remains in `first-join`
  (an independent S printed `native=97` against cargo's 98: not understood, recorded, not waived).
- **Host flags (partly closed, see "The flags for rustc of a script's compile"):** `--cfg` and `-C debug-assertions` of the flags
  a script's compile gets are applied and an explicit `--target <host>` is cargo's cross build; the other flags (codegen options,
  lints, search paths) are named on stderr and **not** applied.
- **Not run at runtime:** the recovery of a stale world for a request that is not an edit (a `sync` that
  loses the race between the check and the pin; component and read only); an edit whose program is built
  from a split that fails to load again; a competing writer already running when a pin is taken (component
  test only); a checkout read as another beyond the code path it shares with `overlay`; an interrupted build
  with a client that needs a group it did not judge (read: the clients give their pins up).
- A build whose script another build ran while it only held it fresh runs the script (its first run of
  it). There is no case of a second execution in a build: the earlier "A/B/A" exception is gone (it was
  the defect of the review of `0e580b5b`). What it costs is rule 1's serialization of the builds that run
  scripts; a daemon without a retire (a hand-started worker whose builds are never retired) keeps the
  admission of an executor for as long as the worker lives.
- A request with no number at all (`build == 0`, a direct run without a daemon) takes the script directory's
  lock itself, as before, and blocks on an executor's admission; in one worker that is the main loop that
  would have to take the executor's retire. The daemon numbers every request it sends and an edit has a
  number of its own, so it was not reached and not run.
- Not rerun: the 42-probe `scriptedits.py` suite, P0-A, the Mac's 234.
