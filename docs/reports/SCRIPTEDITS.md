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

Review of `91d58995` (2026-10-26/27) found two request-lifetime faults in the runner above.
`scripts/leases.rs` answers both; this section says what the lifecycle is, what was chosen and what is
**read**, **measured** (on the Mac, `RUST_BACKTRACE=0`: the lease rules and a two-process component
test; on Linux through the daemon, below) and not shown. Whether they are fixed is for the review
to say; the Linux runs below are the evidence it can read.

### The lifecycle (read)

- A **build** is one `Request::Build` to the daemon (`next_build`). It is several requests to the
  worker, one per program (`Worker::ask`, `RA_JIT_BUILD=<n>`), and is over when the daemon has
  the worker's last answer (`built`). Two clients that ask at once are two builds.
- A script is judged in the worker's main loop by `Checkouts::stale` (`Scripts::changed`, which
  calls `scripts::run`) and by the load (`workspace_at`). `stale` runs before the loop settles the
  builds under way, so a request of build B could rewrite an `OUT_DIR` that build A's compile and
  link had still to read.
- What reads a script's output after `run` returns: the vfs (the files `include!` names), the
  compile (`OUT_DIR`, `rustc-env`, cfgs) and the link (`rustc-link-search` and `-lib` name files
  in `OUT_DIR`). All of them are inside the build.
- `OUT_DIR` is persistent and its path is observable: cargo runs a script again in the same
  directory, the script may read what its last run left, and the path is in what the script
  prints and generates. A copy of the output at another path is not the output the script
  described, and replaying a script's other effects is what #5 did and was parked for.

### What was chosen

Cargo's own answer is to hold the build directory's lock for the whole build. The runner takes
that per script and shares it among the builds that only read:

- a build that judges a script takes a **shared lease** on its run directory (`flock` on
  `blaze-lease`, under the transaction lock) and keeps it until `retire`;
- a build that must run the script takes the lease **exclusively**, runs, and takes it shared
  again before it lets the transaction lock go, so no other run is between;
- what a build judged is kept by (build, run directory): once judged, a script is not judged or
  run again in that build, whatever other builds did between its requests, and a failure is kept
  with its message and delivered by each request;
- the daemon sends `retire <build>` to the worker from a thread of its own, after `built()` returns
  and the reply is on its way to the client (it delays and can hang no client); a worker that
  ends releases its leases with its descriptors;
- a run that finds the lease held by another build **of the same worker** is put off
  (`scripts::Busy`; the main loop asks again after `Event::Retired`), because only that build's
  end frees it; one held by **another worker** is waited for outside the transaction lock
  (default 600 s, `RA_JIT_SCRIPT_LEASE_WAIT_SECS`), as cargo waits for the build directory.

Concurrency kept: builds that find a script fresh (the same watched values) read its output
together and wait for nothing. A build waits only when it must write the output that another
build still reads, which cargo does for every build.

### What it does not do

- A request that names no build (`build == 0`) keeps nothing: it is judged afresh and holds only
  the transaction lock, as before. The daemon always names one.
- If a worker never hears `retire` (daemon killed between answer and message) its leases last
  until it ends, and other workers wait for the deadline and fail with a message. Two workers that
  each hold a script the other must run also wait for the deadline: ordering the scripts by run
  directory would prevent it and was not done.
- A worker blocks its main loop while it waits for another worker's build.

### Tests (measured, Mac, 2026-10-27)

`scripts/leases.rs` has 5 tests (the lease rules, with two lease sets standing for two processes;
`flock` conflicts between open descriptions as it does between processes). `scripts/tests.rs` has
5 new tests and one helper test: an A/B/A counter test with an always-stale script (B is put off
while A reads, A's second request runs nothing, B then runs once), the same with a failing script
(A's failure is delivered unchanged by each later request and the script is not run again), a
cross-worker test that a judgment that stands is not taken for the output another worker wrote
after it, one that builds that find a script fresh read it together while a build that must run it
waits for both, and a two-process barrier test: worker A's script is done, worker B (a second
process, another `GEN`) asks and is held, A reads its own generated file, `cfg`, `rustc-env`,
`-l` and `-L`, A is retired, B runs and finds its own. They run the real runner, run and record
files and a shell script as the build script; compiling the script is the one part left out.

| Command (`CARGO_TARGET_DIR` shared, heavy lock, watchdog, `RUST_BACKTRACE=0`) | Result |
|---|---|
| `cargo +1.99.0 test -p ra-jit --bin ra-jit scripts::` | 19 passed (the 8 from before and 11 new, the barrier test's helper included), 0.8 s |
| `cargo +1.99.0 test --no-fail-fast -p ra-jit` | 234 passed, 0 failed |
| `cargo +1.99.0 test -p blaze-build-script` | 47 passed, 2 failed: `a_non_utf8_checkout_is_still_listed_from_git` and `git_listed_paths_do_not_need_to_be_utf8`, `EILSEQ` from creating a non-UTF-8 file name on APFS. Not changed here; they passed on Linux in the review above. |
| `cargo +1.99.0 fmt --all -- --check` | passed |

Mutation check (once, not committed): with the lease protection removed and a build's judgment
kept for the last build only (the behaviour before this change), the A/B/A counter test and the
failing-script test fail and the others pass, as the cross-process tests rest on `flock` alone.
This is the old behaviour restated, not the old code run.

### Linux, through the daemon (measured, 2026-10-27)

Source `1d895248` (code `167e5fdb` plus harness commits), Linux aarch64, Docker `rust:1.99.0-bookworm`,
one container with `--memory=6g --memory-swap=6g`, release build of `ra-jit` (2m20s), jobs 6,
`RA_JIT_MEMORY_MAX=5G`, `RA_JIT_OWN_BASE_MEMORY=3G`, `RA_JIT_SPARE=0`, `RUST_BACKTRACE=0`; container
and volumes removed afterwards. Binary sha256 `e13d4741…` (in the evidence).

| Check | Result |
|---|---|
| `cargo test --release -p blaze-build-script`, untouched | 49 passed, 0 failed (the Mac's two APFS failures are the file system's) |
| `cargo test --release -p ra-jit --bin ra-jit scripts::` | 19 passed |
| `harness/leasebarrier.py` `same-worker`, `cross-worker`, `failure` | passed, run twice (the second run in the same container) |

`harness/leasebarrier.py` builds a package of two programs with the daemon (`cargo ra-jit build`,
`RA_JIT_WHOLE_BUILD=1`, blaze's compiler and linker; cargo only builds the oracle). Its script
writes a file the programs `include!`, a `cfg`, a `rustc-env`, and a static archive it compiles
(`cc`, `ar`) whose symbol the programs call: all of the value `GEN`, which the script watches.
The worker is held by files (`RA_JIT_SCRIPT_BARRIER_DIR`; `scripts/barrier.rs`), not by time.

- **Same worker.** A (`GEN=a`) is held after its scripts are judged and before its compile and
  link. B (`GEN=b`) asks. B's checkout is loaded only after A's build (the worker settles first),
  so nothing of B's reaches the script while A is held: the output's hash and the script's run
  count do not change for 4 s. A is let go, builds, and its client is answered; the daemon's
  `retire` is held at the worker. B's two requests are put off (`script-lease: put-off`, the
  `Busy` path), and for 4 s more the output and run count do not change and B's client does not
  end. A's programs, copied before the retire is let go, print `a` for the include, the variable,
  the `cfg` and the archive's symbol, as cargo's do. The retire is let go: `retired released=1`,
  and B runs the script 1.3 ms later (the `Event::Retired` wake-up) and builds. B's include,
  variable and `cfg` are `b`'s; A's copies still print `a`. Script runs: `a`, `b`.
- **Another worker.** Two daemons (`RA_JIT_HOME`), one project and target directory. The same,
  but B (a worker of its own, cold) logs `waits` and runs the script 0.2 s after A's retire (the wait
  polls, up to 250 ms apart); both
  programs are the oracle's byte for byte, archive and all.
- **Failure.** A build with two programs whose script fails (`exit 3`): the two requests of
  each build log one `runs` and no `holds`; the script's runs are `bad` once for each of the two
  failing builds; a build that works between them runs; the client's message is the same each time.

What this does not show:

- **Retire after the answer, from the outside.** The worker's log has no word of when the client
  was answered, and the client's process ends a few ms after its answer. What is observed is that the
  retire is held while the client ends and B has run nothing; that the daemon sends it after the
  reply is **read** from `Request::Build` (it is spawned after `built()` returns).
- **A warm worker does not relink for a library the script now names.** In the same worker
  B's include, variable and `cfg` are `b`'s but `native=97`: the archive's contents are A's. This is
  not the leases: with no second build, one worker, `GEN=a` then `GEN=b` (library named for the
  value) leaves the program A's with exit 0 (`link: … not spliced: gen_value_b is not in the
  executable`), and with one name `GEN=a` then `GEN=c` prints `native=97`, cargo's `99`. The same
  controls give the same result on the binary of `91d58995` built the same way. The harness
  records them as gaps (`link-control`, `archive-control`), not as passes. A is checked exactly:
  its link is what a script of B's overwriting the archive would corrupt.
- **The failure's text.** The client prints the first line of cargo's message
  (`failed to run custom build command for …`); the exit status and the script's standard error, which
  are further lines, are not in what the daemon sends it (a reply is one line). Read, not changed.
- `scriptedits.py` (42 probes) and the P0-A audit were not rerun for this change. `build == 0`
  keeps nothing.

The earlier summary that both findings of review 5661429749 were fixed rests, for the lease and the
retire and put-off path, on these runs on Linux (the Mac runs were of the lease rules and a
two-process component test); for B's link in one worker it does not hold, for a reason that is
older than the change.
