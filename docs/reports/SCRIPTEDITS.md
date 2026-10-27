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

Review `5661429749` of `91d58995` found two request-lifetime faults in the runner above; an
independent review of `ac24007a` (`reviews/p05-pr12-sol-lease-review-ac24007a.md`) found four more in
the first answer to them. This section is the design that answers all six, what ran of it, and what
it does not do. **Whether they are fixed is for the review to say**; what follows is the evidence it
can read, each claim marked **read** (from the code), **measured** (a run, with its source), or **not
run**.

### The lifecycle (read)

- A **build** is one `Request::Build` to the daemon (`next_build`), several requests to the worker
  (`RA_JIT_BUILD=<n>`, one per program), over when the daemon has the last answer; the worker's
  answer follows the compile and link of the program. Two clients that ask at once are two builds.
- A script is judged by the worker's main loop in `Checkouts::stale` (`Scripts::changed`) and in
  `reload` (`workspace_at`, which a changed configuration file makes the next request run), and by
  the load. What reads a script's output after `run` returns: the vfs (the files `include!` names), the
  compile (`OUT_DIR`, `rustc-env`, cfgs) and the link (`-L`, `-l`).
- **Not every consumer is a request that names a build.** A direct `ra-jit <project> --exe` run has
  none and compiles and links in its own process after the load. An editor's `edit` is built by the
  worker for a client that has none and never goes through `stale`. A rebuild nobody asked for
  (`Client::nobody`) is the same. All of them read the output after the transaction lock was let go.
- `OUT_DIR` is persistent and its path is observable: cargo runs a script again in the same
  directory, the script may read what its last run left, and the path is in what the script prints
  and generates. A copy at another path is not the output the script described, and replaying a
  script's other effects is what #5 did and was parked for. So the output is protected where it is.

### The design (read)

Cargo holds the build directory's lock for the whole build. The runner takes two things instead:

- a **lease** on each script's run directory (`flock` on `blaze-lease`), shared by the builds that
  read the output, exclusive for the one that writes it;
- the **admission**, the script directory's transaction lock (`.lock`), owned by a *build* (not by a
  call), kept across the times the build is put off.

The rules (`scripts/leases.rs`):

1. A build takes the admission before it touches a script it has no judgment of, so before any lease.
   One that cannot waits for it holding nothing.
2. A build that has the admission may wait for the readers of a script it must run to be retired.
   Nothing it waits for waits for it: a reader is a build that has judged all its scripts, whose later
   requests are answered from **immutable results kept by (build, run directory)** and need neither
   admission nor lease. That is why two builds that take the scripts in opposite orders cannot wait
   for each other, and why no deadline is needed (there is none).
3. A build that holds leases and finds it needs the admission (its scripts changed during the build)
   is refused at once, not made to wait while it holds what the other build waits for.
4. The admission is let go when an attempt ends in anything but a wait, and when the build is
   retired. The requests put off are asked again then.

Waits are not made in the worker's main loop. `Busy` is the answer, and the request is put off
(`later`). It is asked again when `scripts::generation()` is not what it was when the request was
**read, before the stale check**: a retirement, an admission let go, or a file of another process
freed (a thread polls it, `scripts::watch`) all move it, so none is lost when it comes while the
request is on its way to the queue. A caller that cannot be put off (the load before the worker
serves, a direct run) waits for another process without end, as cargo does.

Every consumer has an owner: a numbered build; the request's own number (`Client::of`, released
when the `Client` is dropped, whatever ended it); a load no daemon started (`scripts::load_build`,
released after its first program is compiled and linked); and an edit or a rebuild nobody asked for
holds the output the process last judged, shared, in its build thread, until it ends. The daemon
asks a worker to retire a build until it hears (a worker that is loading listens only when it has).

Concurrency kept: builds that find a script fresh read its output together and wait for nothing; a
build waits only to write what another still reads, which cargo does for every build.

### Tests

Component (Mac, shared target, heavy lock, watchdog, 20 passed at the final source): `scripts/leases.rs`
(6: the admission is a build's across its waits; another process's admission is waited for and a reader
is refused rather than waiting for it; results are immutable and kept until the build ends; the
generation moves on retire and release; a consumer that judged nothing holds what was last judged until
it is released; the review's opposite orders cannot wait for each other) and `scripts/tests.rs` (A/B/A
counter, A/B/A with a failing script, cross-worker judgment, shared fresh output, and a two-process
barrier, over the real runner, run and record files, with a shell script as the build script;
compiling the script is the one part left out).

Linux through the daemon (`harness/leasebarrier.py`; Docker `rust:1.99.0-bookworm`, one 6 GiB container,
no extra swap, own docker lock, container and volumes removed; `RA_JIT_MEMORY_MAX=5G`,
`RA_JIT_OWN_BASE_MEMORY=3G`, `RA_JIT_SPARE=0` inside it, a guard of the test, not a product limit).
The scripts write a file the programs `include!`, a `cfg`, a `rustc-env` and a static archive they
compile; blaze compiles and links, cargo only builds the oracle; a worker is held by files
(`RA_JIT_SCRIPT_BARRIER_DIR`) at named points. Each scenario was run on the binary of `04721719` with
only the barrier hooks added (a *baseline*, no fix) and on the fixed build:

| scenario | what it holds apart | baseline `04721719`+hooks | fixed, source `913ae474` |
|---|---|---|---|
| `lost-wake` | A's retire arrives while the worker is held between B's put-off and its queue; B is one request | B hangs (client killed after 40 s) | B built with no other request |
| `reload-busy` | a forced `[env] GEN` makes B's request a reload, A not retired | B fails: "output … is in use by build 1" | B put off (`put-off … reload:`), built after the retire |
| `direct` | two direct `ra-jit --exe` runs, A held before its compile and link | B rewrites the archive; **A's own executable prints B's `native=98`** | B logs `waits`; A's executable is cargo's; B built after A ends |
| `coab` | C reads p2; A (-p p1) runs p4's script then needs p2's; B (-p p3) needs p2 fresh then p4's | A fails: "stayed in use by another worker for 30s", B hangs | all three build; the program of each is cargo's |
| `internal` | an editor's `edit` built for a client with no build, held before its compile and link | B rewrites the output while the edit is held | B logs `waits`; the edit logs `holds … loaded scripts=1` |
| `same-worker`, `cross-worker`, `failure` | the earlier scenarios (below) | pass | pass |

On the fixed build the whole harness (10 scenarios) passed in one session on `913ae474`; `lost-wake`,
`reload-busy`, `direct`, `coab`, `same-worker`, `cross-worker`, `failure` and the two controls also passed in an
earlier session on `aa7c7877` (the same lease code). `internal` has one passing run, and the baseline
failure above. In the baseline session of that earlier build `internal` was not exercised (it used the
store's dehydrate path, which a whole build does not take); it was changed to an `edit`. Also on Linux
at `913ae474`: `blaze-build-script` untouched 49 passed, `scripts::` 20 passed. Evidence (pids, worker
logs, build numbers, barrier files, hashes, timeline): `p05r-evidence/lifecycle-913ae474/`.

Order observed (fixed, `lost-wake`): `put-off` build 2 → `barrier-reached deferred` → retire released →
`retired build=1 released=1` → `barrier-released deferred` → `runs build=2`. `coab`: runs `x:x1 y:y2 x:x2 x:x1
y:y3`. Retire-after-the-answer is not observable from outside (the worker's log has no word of the
client's answer); it is read: the daemon spawns the retire after `built()` returns.

The first same-worker/cross-worker/failure scenarios (all earlier `04721719` and `ac24007a`
evidence): in one worker B is put off for the retire; in two, B waits and runs 0.2 s after A's retire;
a failing script runs once per build for two requests with the same message.

### What is not shown, not fixed, or wider

- **A warm worker does not relink for a library the script now names** (`link-control`,
  `archive-control`, and B in `same-worker`/`lost-wake`/`reload-busy`): B's include, variable and `cfg` are
  `b`'s but `native` is A's, with exit 0 (`link: … not spliced: gen_value_b is not in the executable`).
  It happens with no second build and on the binary of `91d58995`. Recorded as gaps by the harness, not
  passes, and **not fixed here: a named blocker**. A's link, which a lease violation would corrupt, is
  checked exactly.
- **Inherited, outside this change, a blocker and not an accepted limit:** the script's compile keys
  `host_flags` but the owned compiler is not given `--cfg` (or the other flags), and explicit
  `--target <host>` is taken for none. A `build.rs` with two `main`s chosen by `RUSTFLAGS="--cfg chosen"`
  compiles the wrong one. Needs a differential test of the real script executable.
- A failing script's client message is its first line (a reply is one line); the exit status and
  standard error are not in it.
- A build that must run a script when its own scripts changed during the build (a package set that
  changes in a reload) is refused with a message, not made to wait.
- A worker whose daemon never sends `retire` (the daemon killed between the answer and the message)
  holds the build's leases until it ends; other workers wait without end for it.
- Edits and rebuilds nobody asked for hold the scripts the process **last judged**; if another
  worker has rewritten them since the worker loaded, the worker's database and the disk differ, as they
  did before. The lease makes the output not change *while* the consumer reads it, not coherent with a
  database loaded earlier.
- Not rerun for this change: the 42-probe `scriptedits.py` suite and the P0-A audit.
