# Why every edit of tracedecay's CLI went to cargo: its build script printed something else

Unit u8-buildscript of the distributed compile, cache and tests program. The question:
[TRACEDECAY.md](TRACEDECAY.md) (items 4 and 7) found that 18 of 18 builds after an edit went to cargo
because "the build script of `tracedecay-cli` printed something else" the second time blaze ran
it, and guessed the cause was the script reading git's state (`build.rs:417-422`, **unverified**).
What is it, and which side is wrong, blaze or tracedecay?

**read** means a number or line was read from a run here, with the run count. **inferred** means
it follows from what was read. Nothing is extrapolated.

## The short version

**blaze is wrong, and tracedecay's script is fine.** The cause is not git's state.

- **What differs.** When the build script's output is compared, blaze runs the script again into
  an **empty `OUT_DIR`**, where cargo runs it in the **same `OUT_DIR`**, with the frontend bundle the
  first run left there. In an empty directory the script has no record of what the bundle was built
  from, so it runs `pnpm run build`. rsbuild prints 61 lines to the script's standard output
  (`Rsbuild v2.2.13`, `ready built in 0.77s`, a table of 52 staged files with their sizes). Cargo
  reads only lines that start with `cargo:`; the others are the script's own and it ignores them.
  blaze compared every byte, against the output cargo kept from its own last run, which has none of
  those lines. 55 of the 61 lines differ from the first run's as well: the time, the path (it names
  the `OUT_DIR`, relative to `dashboard/`, and so is not the path blaze rewrites) and the width
  of the table's first column, which follows the path.
- **Cargo has the same lines, once.** Cargo's first run of the script prints the 61 lines. Its
  reruns print none, and are byte for byte the same as each other (3 reruns, 877,830 bytes each).
  The script's output depends on the state of its `OUT_DIR`, which cargo leaves as it was between runs
  (**read**: the first rerun found the bundle and did not run pnpm).
- **Fix: in blaze** (`crates/ra-jit/src/cargo/script.rs`). The daemon runs the script in a copy of
  the `OUT_DIR` cargo left and compares the lines cargo reads, as cargo reads them. With the
  fix the scratch project that has tracedecay's own `build.rs` goes from 0 of 3 edit builds the
  daemon's to 3 of 3 (read, 1 run each of `blaze build`), and the harness case that has the script in
  miniature from 0 of 3 to 3 of 3. tracedecay needs no edit; an optional patch that keeps pnpm's lines off the
  script's standard output is in the last section.

## What was run

| | |
|---|---|
| Machine | Apple M5 Pro, 18 cores, 48 GiB, macOS 26.6.2. The machine was not quiet: other units' builds ran (load average 7.4 to 36.7 at the start of the runs) |
| blaze before | the fork at `c8e5f6a8` (`/Users/bytedance/dev/blaze-targets/u1/release`); `dist/main` (`a67ea8db`) differs from it by docs and `harness/tracedecay.py` only (`git diff c8e5f6a8 origin/dist/main --stat`) |
| blaze after | this branch, `cargo +1.99.0 build -p ra-jit --release`, 2 m 10 s (1 run), in `/Users/bytedance/dev/blaze-targets/u8/release` |
| tracedecay | `d79de82273` in a detached worktree of the owner's checkout, `td-u8`, with `#![recursion_limit = "256"]` in `tracedecay-code-index-runtime` as in [TRACEDECAY.md](TRACEDECAY.md), `pnpm install --frozen-lockfile --offline` run in it. Nothing was written in the owner's checkout |
| The script | the unchanged `crates/tracedecay-cli/build.rs` and `build-support/`, compiled by cargo 1.99.0 as the build script of a scratch package, `td-u8/local-only/u8bs` (`local-only/` is gitignored, and the script finds the repository at `../..` from its package, as for the CLI). Its build dependencies are the CLI's (`flate2`, `serde_json`, `sha2 0.11.1`). Its `[package]` has no dependencies, so nothing but the script is built. The whole of tracedecay does not fit this Mac under blaze ([TRACEDECAY.md](TRACEDECAY.md) item 5) and is not needed to see the script |
| Rust | 1.99.0 for cargo and blaze |

Every build of blaze, every run of the build script and every blaze daemon run held the heavy lock (the compile of
the scratch package's build script, with a `pnpm` that fails at once, and `pnpm install` did not); every blaze run had `RA_JIT_MEMORY_MAX=16G`,
`RA_JIT_OWN_BASE_MEMORY=12G`, `RA_JIT_SPARE=0` and the harness watchdog (24 GB).

## 1. The two outputs

Cargo runs the script, then runs it again after a tracked file of the repository has changed (the
script watches every file `git ls-files` lists, 6,734 of them here). One cargo 1.99.0 build per
step, `cargo build --offline`, into one target directory; `output` is the file cargo keeps beside
`OUT_DIR` (`E1`, 1 run of 4 builds):

| Step | `output` of the script | Lines | Lines that do not start with `cargo:` |
|---|---|---|---|
| 0, cold build | 887,181 bytes | 6,801 | **61** |
| 1, after an edit | 877,830 bytes | 6,740 | 0 |
| 2, after another | 877,830 bytes | 6,740 | 0 |
| 3, after another | 877,830 bytes | 6,740 | 0 |

The 6,740 lines of steps 1 to 3 are 6,734 `cargo::rerun-if-changed` and 6 `cargo::rerun-if-env-changed`.
The script prints no other directive: nothing for rustc. The `cargo:` lines of step 0 are the same
6,740, in the same order (`cmp`).

The 61 lines of step 0 are the output of `pnpm run build` (rsbuild), which the script starts with
`Command::status()`, which inherits the script's standard output (`build.rs:292-298`, the `run_pnpm`
function):

```
Rsbuild v2.2.13

info    build started...
ready   built in 0.77s

File (web)                                  Size        Gzip
../target/u8-e1.noindex/debug/build/u8bs-1e11a47fc7e7b31a/out/dashboard-bundle/staging/index.html   0.55 kB     0.30 kB
... 51 more rows ...
                                            Total:   3533.6 kB   1236.0 kB
```

Now blaze's side. The daemon takes the environment (103 variables) and the directory cargo ran the
script with (`ra-jit-ran.json`, written by `ra-jit build-script`), sets `OUT_DIR` to an empty
directory beside cargo's and runs the script (`rerun_into`). The two reruns below are the script run by
hand with the environment cargo gave it at step 2 (`E2`, 1 run each), compared with cargo's `output`
after step 3:

| Rerun | Exit | Time | Standard output against cargo's `output` | `OUT_DIR` against cargo's |
|---|---|---|---|---|
| in an empty `OUT_DIR` (what blaze does) | 0 | 1.8 s | **different**: 887,883 bytes, 61 lines that are not directives; 55 of them differ from step 0's | same digest (125 files) |
| in a copy of cargo's `OUT_DIR` (what cargo does) | 0 | 0.9 s | **the same bytes** (877,830) | same digest |

The 55 lines that differ from step 0's, as `diff` prints them, are in three places: `ready   built in
0.77s` against `built in 0.51s` (the time); the table's header and each of its 52 rows, because rsbuild
prints the staged path relative to `dashboard/` (`../target/.../out/dashboard-bundle/staging/...` against
`.../ra-jit-out-fresh/dashboard-bundle/staging/...`), which blaze's rewrite of its directory's absolute
name does not match; and the `Total:` line, which is padded to the width of the longest path. The sizes
are the same, and so is the bundle (digest `771c39c0...`, the same in `td-u8` and in u5's `td-u5` cold
build, read from its `out/dashboard-bundle/`).

## 2. Why they differ

| Candidate | Same under blaze's rerun and cargo's? | Evidence |
|---|---|---|
| Environment | Same: blaze runs the script with the variables cargo gave it (103; `env_clear` then `ran.env`). The jobserver in `CARGO_MAKEFLAGS` points at descriptors the daemon does not have; the rerun does not need them (**inferred** from its exit 0 and its output) | `ra-jit-ran.json`; the rerun above exits 0 |
| Working directory | Same: `ran.cwd`, the package's directory | `ra-jit-ran.json` |
| Git state | Same: the edit does not add or remove a file, so `git ls-files` is the same list; the tree was dirty before and after (`recursion_limit`); no untracked file appeared after any step (`git ls-files --others --exclude-standard` empty, 5 reads). `PRODUCT_SOURCE_DIRTY` is in the generated module, which is a file, not output | step lines of `E1` |
| Time | Differs, in the one line `ready built in Ns` | 1 line |
| **`OUT_DIR`** | **Differs: empty for blaze, the first run's for cargo.** The script keeps `dashboard-bundle/built-from` there (the fingerprint of the frontend's inputs) and builds only when it is not what the bundle was built from (`build.rs:217-221`) | the 61 lines are there for the empty directory and not for a copy |

So the difference is `OUT_DIR`, and the time and the path in the lines are two of the ways the 61 lines
differ from step 0's. What git's state does to this script is different and is not the cause: it
changes the file list, and so the `rerun-if-changed` lines, and neither the edits nor the reverts of
[TRACEDECAY.md](TRACEDECAY.md) do that.

Why 18 of 18 and not some: cargo's last run, the one blaze compared with, was in the same directory as
the bundle, so after the first rerun its output never has the 61 lines, and blaze's run in an empty
directory always has them. Each edit that blaze sent to cargo had cargo rerun the script in its own
directory, which left `output` without the lines again. The first edit after a cold build (whose
`output` has the 61 lines) differs from the daemon's run in them too, by the time and the paths.
Run A of [TRACEDECAY.md](TRACEDECAY.md) item 3, which counted the message once during a cold build,
is consistent with the same mechanism (the check runs when a watched file was written after cargo's
run; **inferred**, its log is gone); why run B had none is **not explained**.

## 3. Is the script nondeterministic under cargo too?

Its first run is, in lines cargo ignores: the 61 lines have the time and the paths. Its reruns in the
same `OUT_DIR` are not (3 reruns, the same 877,830 bytes, `cmp` equal), and nothing cargo reads differs
between the first run and the reruns (the 6,740 `cargo:` lines, `cmp` equal). Cargo does not compare
the output of one run with another's at all: it runs the script when a path it was told to watch is
newer than the last run, and uses the new output. blaze's check asks something cargo never does
("is it the same as last time?"), and it asked with a stricter reading of "the same" than the one cargo
has of what a script prints.

## 4. The fix

In blaze, because both halves of the difference are blaze's:

1. **Run the script in a copy of cargo's `OUT_DIR`**, as cargo does. `rerun_matches` copies the
   directory (files and directories with their times and permissions; see "After review" for the
   links) to the directory it runs the script in, so a script that keeps what it built there finds
   it. cargo's directory is not written to, which is why it was a directory of its own (compilers
   under way read cargo's; cargo builds the package again when a file there is newer). Copying
   `td-u8`'s 125 files, 4.5 MB, took under a second (the cost of the reviewed version is in "After
   review").
2. **Compare the lines cargo reads.** `directives()` keeps the lines that, as text and trimmed,
   start with `cargo:` (`cargo::` too), which is where cargo's `BuildOutput::parse` starts (and
   [`blaze-build-script`'s `output::parse`](../../crates/blaze-build-script/src/output.rs)). What the script
   or a tool it runs says besides is the script's own. `rerun_into` and `same_run` compare those, in order.
   What the script wrote into `OUT_DIR` is still compared whole (`tree_digest`, which is `digest_of`
   of the first version, made to read links and failures as "After review" says), so a script
   whose files change still sends the build to cargo.

Neither step makes a build the daemon's that cargo would not have built the same. A script that prints
a different directive (a `rustc-cfg`, `rustc-env`, a link argument, another `rerun-if-changed`), or
writes a different file, is still cargo's, with the same message.

## 5. Checks

### The scratch project under `blaze build` (E3)

`blaze build --locked -p u8bs` in the scratch package, cold, then three edits (a tracked file of
`td-u8` and the package's `lib.rs`, so that the daemon has something to compile and the script's
watched files are newer), under the guard and the watchdog, 1 run of each (n=1; the harness's records are
in `u8-work/e3-before` and `e3-after`):

| | cold | edit 1 | edit 2 | edit 3 | `blaze status` afterwards |
|---|---|---|---|---|---|
| fork `c8e5f6a8` | the daemon's, 113.0 s | **cargo's** 12.7 s | **cargo's** 41.4 s | **cargo's** 6.4 s | `1 builds by the daemon, 3 by cargo`, `3 x the build script of u8bs printed something else` |
| this branch | the daemon's, 62.1 s | **the daemon's** 1.62 s | **the daemon's** 1.81 s | **the daemon's** 1.55 s | `4 builds by the daemon, 0 by cargo` |

The first line of each of the fork's three edit logs is `ra-jit: bug: the build script of `u8bs`
printed something else; building with cargo, which ra-jit is never to do (no-rustc M, #93, and #84)`,
word for word the line of the 18 edit logs of [TRACEDECAY.md](TRACEDECAY.md). With the fix the logs
have no such line and end `Finished `dev` profile [ra-jit]`. The wall times are not a comparison of the
two blaze builds: the fork's edits were cargo's builds (with the daemon's workers loading
again) and ran while other units' builds loaded the machine (load average at their start 16.8, 14.9
and 11.6, against 8.0, 7.9 and 7.4 for the branch's; no other build in the edit runs themselves, peak
0.0 cores of other builds); the point is who built them.

### Unit tests

`cargo +1.99.0 test -p ra-jit` (debug, 1 run, 2 m 2 s with the build): **197 passed, 0 failed**, in the
`ra-jit` binary's tests; the other targets have none. That is the 193 of `dist/main` and the 4 added in
`cargo::script::tests` (**inferred** from the names; `dist/main`'s own count was not run). The 9
script tests that were already there pass with the change. The two tests that run a script
(`a_rerun_finds_in_its_directory_what_the_last_run_left_as_cargos_does` and
`what_a_script_says_that_cargo_does_not_read_is_not_compared`) **fail on the old comparison**:
with the three lines of `rerun_matches`, `rerun_into` and `same_run` put back (a rerun into an empty
directory, `bytes == bytes`), both stop at `left: Err("printed something else")` (`cargo test -p ra-jit
--bin ra-jit -- cargo::script`: 11 passed, 2 failed). The other two new tests (`directives`, `copy_tree`)
test the functions the change adds and cannot run on the old code.

### The harness case (`harness/scriptrerun.py`)

A project with a build script of the CLI's kind (it hashes `inputs/`, records the hash beside the
bundle it writes into `OUT_DIR`, runs a tool that says how long it took, which process it was and
where it wrote, and lists every file as `rerun-if-changed`), built with `cargo ra-jit build`, under
the guard and the watchdog (peak 2.46 GB), 1 run of each:

| Step | fork `c8e5f6a8` | this branch |
|---|---|---|
| an edit of the program | cargo's: the build script of `app` printed something else (**WRONG**) | the daemon's |
| an edit of a file the script watches | cargo's: printed something else (**WRONG**) | the daemon's |
| another edit of the program | cargo's: printed something else (**WRONG**) | the daemon's |
| an edit of an input the script bundles | cargo's: printed something else; the program prints the new bundle | cargo's: wrote something else; the program prints the new bundle |

The program's output is cargo's in every step of both runs. The last step is cargo's on purpose: the
script writes another bundle, and the daemon does not build what a script wrote anew. With the fix
the run exits 0 in 13.8 s; against the fork it exits 1 after the three steps (23.8 s). The harness is in
`harness/ci.py` (part `core`, quick tier 1, Linux and macOS) and `docs/harness.md`; `harness/imports.py`
imports it (119 of 128 scripts import on this Mac's Python 3.9; the nine that do not need `tomllib` or
a newer syntax and are not this change's).


## 6. The optional patch for tracedecay

None is needed. The script follows cargo's contract: cargo reads the lines that start with `cargo:`
and leaves the rest alone, and it runs the script in the same `OUT_DIR` every time. A check that
compares everything a script prints, from a directory cargo never gave it, asks for more than the
contract says. blaze is the one to change, and did.

One thing tracedecay could do is still worth saying, because it makes the script's standard
output hold what cargo reads and nothing else (a build script's standard output is the channel to
cargo, and pnpm's lines on it are noise there). `run_pnpm` starts pnpm with `Command::status()`,
which has pnpm inherit the script's standard output. Capturing it and writing it to standard error
keeps the lines for a person (`cargo build -vv` shows a script's standard error, and a failed
build prints it) and makes the first run's output the same as every rerun's. This patch,
against `d79de82273` (`crates/tracedecay-cli/build.rs`), was applied in `td-u8` and run as E1 was
(cold build and 3 reruns, then both reruns of blaze's check; E4, 1 run, since reverted):
the cold output was 877,830 bytes with 0 lines that are not `cargo:` (887,181 and 61 without it),
and blaze's old rerun in an empty `OUT_DIR` printed the same 877,830 bytes, in 1.6 s, with the same
`OUT_DIR` digest. So with this patch even the fork's check at `c8e5f6a8` would have passed on this
script. It is not the fix: the next script that keeps its tool's noise on standard output would
meet the check again, and the old check ran the frontend's tool again each time (1.6 s here against
0.8 s for the rerun in a copy; the CLI's dashboard is built in under a second on this Mac,
`ready built in 0.77s`).

```diff
--- a/crates/tracedecay-cli/build.rs
+++ b/crates/tracedecay-cli/build.rs
@@ -295,11 +295,11 @@
             .args(args)
             .envs(envs.iter().copied())
             .current_dir(dir)
-            .status()
+            .output()
     };
     // Standalone pnpm installs `pnpm.exe`; npm and Corepack install a
     // `pnpm.cmd` shim, which `Command` does not resolve on its own.
-    let status = match spawn("pnpm") {
+    let output = match spawn("pnpm") {
         Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::NotFound => spawn("pnpm.cmd"),
         result => result,
     }
@@ -310,6 +310,17 @@
             args.join(" ")
         ))
     })?;
+    // A build script's standard output is Cargo's: it reads the lines that start
+    // with `cargo:` and keeps the rest unread. pnpm's own lines (how long the
+    // bundler took, the staging paths it wrote) differ on every run, so they go to
+    // standard error with the rest of what pnpm printed.
+    {
+        use io::Write as _;
+        let mut stderr = io::stderr().lock();
+        let _ = stderr.write_all(&output.stdout);
+        let _ = stderr.write_all(&output.stderr);
+    }
+    let status = output.status;
     if status.success() {
         Ok(())
     } else {
```

## What was not measured

- **The whole of tracedecay under blaze.** It does not fit this Mac ([TRACEDECAY.md](TRACEDECAY.md),
  items 5 and 7). The script, its environment and its files are tracedecay's; the daemon's load of
  the 586 crates, the 107 build scripts it fails to compile on macOS arm64 and what a loaded daemon
  does with an edit of `tracedecay` or `tracedecay-mcp` are not run here. With the fix, **the
  first reason** the 18 edits went to cargo is gone; whether the daemon then builds each of them
  depends on the load finishing within the machine's memory (item 7, gaps 1 and 2) and is **unverified**.
- **The gate.** `harness/ci.py core` (the other harnesses), the LLVM backend, `cargo test --release`
  of the other crates and `cargo fmt --all -- --check` under CI's toolchain were not run; the last was
  run with 1.99.0's `cargo fmt` and passes. Only `script.rs` of any crate changed.
- **Linux.** The scratch project, E1 to E4 and the new harness ran on macOS only. The fix has no platform code beyond
  the copy of links, which Windows leaves to cargo, and of directories, whose times Windows does not
  let the copy set (`File::open` of a directory), so a rerun there is cargo's too (**inferred**, not run on Windows).
- **A script that writes the name of its `OUT_DIR` into a file** is still told apart from cargo's run
  (`wrote something else`), as before: the copy of the directory has the old name in it and the script
  writes the new one. Whether the CLI's does: the 125 files of its `OUT_DIR` have the same digest, so it does not.
- **Run A and run B of [TRACEDECAY.md](TRACEDECAY.md)** (why the message once appeared during a cold
  build and once did not): the u1 target directories are gone.
- **A size limit on the copy.** A script whose `OUT_DIR` holds gigabytes (a C++ library built there)
  is copied in full at each check, and read three times (see "After review" for the cost at 4.5 MB). The
  cost at that size was not measured, there is no limit and no time limit on the script's run, and a
  run in an empty directory builds that library again, which is more.
- **Cost of the daemon's other work on this script.** `ra-jit build-script` took over a minute after
  the CLI's script ended in the cold build of E3, in `Kept::of` hashing each `rerun-if-changed`
  directory and file for the store (one stack sample of the running process: 602 of 785 samples in
  `digest_of`, 160 in `watched_digest`; **read**, n=1, load average 24). It is not part of this fix
  and is a follow-up.

## After review (unit u8-fix)

A verifier (GPT 6.1; its report is not in the repository) failed the first version of this fix
with three findings in `script.rs` and a weak gate in `harness/scriptrerun.py`. All four are
real and are fixed on the same branch. Each has a test that fails on the first version and
passes now: `cargo +1.99.0 test -p ra-jit --bin ra-jit -- cargo::script` on `182389d8`'s
`script.rs` with the new tests added and nothing else gave **18 passed, 10 failed** (1 run), and
the 10 are the tests named below as failing before (one of them is a hang that the test gives up on
after 20 s); on this branch the same tests give 29 passed, 0 failed, 1 ignored (see "How the tests
were run").

| # | Finding | What the first version did | Now | Test (fails before) |
|---|---|---|---|---|
| 1 | Times and permissions | Directories were created new: a script that tests whether a directory is older than an input (`make` style) found it new, did not make its output again, and the rerun was "the same". The mtime of a file was set best effort: a read-only file lost it on Linux (**inferred** from the code, which ignored the error: not run on Linux) | Every directory is given the times and permissions of the original after what is in it (a read-only directory is made writable again only to remove the copy), every file is copied through one handle and given its times and permissions on it, and a failure of any of this is an error: the run is cargo's | `a_directory_of_the_copy_is_as_old_as_cargos_is`, `a_directory_of_the_copy_has_the_permissions_of_cargos_and_is_removed_with_them`; `a_read_only_file_of_the_copy_is_as_old_as_cargos_is` passes on the first version **on macOS** (`fs::copy` keeps a file's time there) and is the case for Linux |
| 2 | Links | A link was copied as it was unless it was an absolute path below the directory: `../state`, an absolute path out of the directory, and `../out/data` (the original by another name) all led from the copy to cargo's own files, so a script wrote there, and the comparison, run after, found both sides the same | Each link is resolved (`canonicalize`). If it ends below the directory it is made a link to the same place below the copy; if it ends anywhere else, or nowhere, or the links go round in a circle, the copy is an error and the run is cargo's. A file that is no file, directory or link (a pipe) is an error too: the copy waited for a writer. The digest before the run is taken from the copy before the script starts, never after, and cargo's directory is read after the run too, for its contents and (a `stat` of everything in it, no read) its sizes and times, and has to be as it was, so a write that reached it by a way not seen is found, one of the same bytes too, which makes its file newer. The digest does not follow links (a link to the directory itself was an endless tree), records where each one leads, and fails on what it cannot read, where it read nothing and called that equal | `a_link_that_leads_out_of_the_directory_is_not_written_through` (relative and absolute), `a_link_that_leads_back_in_by_another_name_is_written_through_in_the_copy`, `a_link_that_leads_nowhere_or_in_a_circle_is_not_run_through`, `a_link_the_script_points_elsewhere_is_a_difference`, `what_the_script_left_is_compared_with_what_was_there_before_it_ran`, `cargos_directory_is_not_written_to_even_with_what_it_has`, `a_file_that_is_not_a_file_is_not_copied_for_a_run`; kept as controls that pass on both: `links_inside_the_directory_are_links_to_the_copy`, `a_link_to_the_directory_itself_is_fine` |
| 3 | Lossy decoding | `String::from_utf8_lossy` ran before the lines were picked, so a line with a byte that is not UTF-8 (`KEY=\xff`) became `KEY=U+FFFD`, equal to a valid old line, where cargo skips the whole line and has no `KEY` | The lines are picked from the bytes, as `BuildOutput::parse` does (cargo `src/compiler/custom_build.rs`, 933 to 961 at `5f94df478`, the reference the verifier read): a line that is not valid UTF-8 is skipped whole, a valid one is trimmed, `cargo:` and `cargo::` both count, order and repeats are kept; the name of the directory is replaced in the lines picked, so in both callers (`rerun_into`, `same_run`) | `a_line_that_is_not_text_is_not_a_line_cargo_reads_in_either_run` (both callers), `only_the_lines_cargo_reads_are_directives`, `the_name_of_the_directory_a_script_ran_in_is_replaced_in_the_lines_cargo_reads` (these last two use `directives_named`, which the first version lacks, and cannot run on it) |
| 4 | The gate | The harness compared the program's standard output only, so a program that exited differently, or wrote to standard error, or a reference `cargo run` that failed (both print nothing), passed | `differences()` compares exit status, standard output and standard error (as `program_output` of `cargo_ra_jit.py` reads them), and fails when the reference run failed. `selftest()` runs first, and alone with `--selftest`, over six cases, 4 of them the old check passed | `python3 harness/scriptrerun.py --selftest`; the old check (`ran.stdout != theirs.stdout`) misses 4 of the 5 wrong cases, the new one finds all 5 (run here) |

The verifier's missing-directory and failed-script cases (`a_directory_that_is_missing_is_an_error_and_leaves_no_copy`,
`a_script_that_fails_is_an_error_and_leaves_no_copy`) pass on both versions; they stay as tests of the
clean-up, which now looks for any `ra-jit-out-*` beside the directory (the copy's name has a counter
now, so two checks in one process do not share a directory). One existing test,
`a_rerun_leaves_what_cargo_has_alone`, took its time for "a second ago" after the check it was
timing and failed when the machine made the check take more than a second; it takes it before.

### Numbers

| | Count |
|---|---|
| `cargo +1.99.0 test -p ra-jit` on `182389d8` (the verifier's run) | 197 passed, 0 failed (read from the verifier's report; not re-run here) |
| the same on this branch | **213 passed, 0 failed, 1 ignored** (1 run, debug; the ignored one is the measurement below). `cargo::script::tests`: 13 before, 29 run and 1 ignored now |

**Cost of the copy** (n=10 per row, one session, an Apple M5 Pro that was not quiet: load average 3 to 5
and other units' processes; release build; `cargo test --release -p ra-jit --bin ra-jit -- --ignored
--nocapture the_cost`). The directory is **not** tracedecay's: its `td-u8` worktree is gone, so it is
a made-up one of the size the first version of this report measured for it (125 files, 4,530,000
bytes in 7 directories, random bytes), and the script does nothing, so the figures are the cost of
the check and not of a script. `rerun_is_same` as a whole, the first version and this one measured
in the same session, two rounds each:

| | min | median | max |
|---|---|---|---|
| first version (`182389d8`), round 1 | 515 ms | 603 ms | 1,000 ms |
| first version, round 2 | 589 ms | 618 ms | 1,206 ms |
| this branch, round 1 (before the look at cargo's sizes and times was added) | 259 ms | 357 ms | 641 ms |
| this branch, round 2 (the same) | 169 ms | 341 ms | 508 ms |
| this branch, final code | 178 ms | 361 ms | 576 ms |

By step on this branch (final code): copy 55 ms median; hash of the copy before the run 250 ms (min
85, max 343); the script 5 ms (it does nothing); hash of the copy after 13 ms; hash of cargo's 14 ms;
removing the copy 26 ms; the `stat` look at cargo's, twice, is in the whole and was not timed alone. The limiter is the first read of a file that was just written, which costs
about 1.5 to 2.5 ms a file here and 0.1 ms after the first (a scratch program, 125 files, 10 runs: reopening a
copied file by path to set its time cost 330 ms, setting it through the handle that wrote it
36 ms for the whole copy). That is why the files are copied through one handle. What on this Mac
does it was not isolated; the machine runs endpoint-security extensions, which is a **guess**. Linux was not measured. The
first version's cost is, by this, mostly that reopening (**inferred**; its steps were not timed
alone), and this one is cheaper than it was though it reads the tree three times (before, after,
and cargo's) where the first read it twice. A directory of
gigabytes is not measured and has no limit; it costs a copy, three reads and a removal at each check.

Not done: a limit on the size of the copy or the length of the script's run (the verifier's note,
not a finding); hard links inside the directory are copied as separate files, so a write through
one name that the script then reads by another is in the copy as two (not tested: **inferred** that
the comparison still finds a difference when the script writes anything different); a script that
finds cargo's directory by its own path (`$OUT_DIR/../out`) writes there, which the copy cannot
prevent, and the look at cargo's after the run finds it and makes the check an error, after the
write (`cargos_directory_is_not_written_to_even_with_what_it_has`); the full tracedecay run is still
not made (this Mac, as above). Windows: the copy of a directory's times and of links is cargo's
there (**inferred**, not run).

### How the tests were run

Every run held the heavy lock (`with-heavy-lock.sh`) and the guard (24 GB sum of top MEM, swap
growth), `CARGO_TARGET_DIR` of its own, `RA_JIT_MEMORY_MAX=16G RA_JIT_OWN_BASE_MEMORY=12G
RA_JIT_SPARE=0`, `RUST_BACKTRACE=0`; peaks 0.9 GB (unit tests), 1.9 GB (release build), 2.6 GB
(harness). No swap growth in any run. "The first version" for the failing run is the new tests added
to `182389d8`'s `script.rs` and nothing else (the ones that use only `rerun_is_same`, `rerun_matches` and
`same_run`, which that version has); the cost rows are the first version's `script.rs` with a
timing test added, and then this branch's, restored byte for byte (`sha256` checked). The harness:
`RA_JIT_BIN=.../release/ra-jit python3 harness/scriptrerun.py` on this branch's release build
passes (the first three edits the daemon's, the fourth cargo's with "wrote something else", the
program as cargo's in all four, its exit status and standard error too).

## Commands

All in this checkout unless a path says otherwise. `H` is the heavy lock of standing order 15 (taken
per run, released by a trap), `G` is the guard (`RA_JIT_MEMORY_MAX=16G RA_JIT_OWN_BASE_MEMORY=12G
RA_JIT_SPARE=0`) under the harness watchdog.

```
git -C /Users/bytedance/dev/tracedecay worktree add --detach /Users/bytedance/dev/td-u8 d79de82273
# in td-u8: #![recursion_limit = "256"] at the top of crates/tracedecay-code-index-runtime/src/lib.rs;
#           pnpm install --frozen-lockfile --offline   (3 m 8 s);  .pnpm/crates/config.toml moved to .off
# scratch package td-u8/local-only/u8bs: Cargo.toml with build = "../../crates/tracedecay-cli/build.rs",
#   build-dependencies flate2 =1.1.10, serde_json =1.0.154, sha2 =0.11.1, edition 2024, [workspace]
# H: E1 and E2, cargo 1.99.0 only (the four builds and the two reruns of section 1)
python3 -I exp1.py
# H, G: E3, blaze build on the scratch package, cold and 3 edits, with the fork's and this branch's binaries
python3 -I e3.py /Users/bytedance/dev/blaze-targets/u1/release u8-work/e3-before 3
python3 -I e3.py /Users/bytedance/dev/blaze-targets/u8/release u8-work/e3-after 3
# H: E4, the optional patch applied to td-u8's build.rs, then E1 again, then reverted
# H: the build of this branch
CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8 cargo +1.99.0 build -p ra-jit --release
# H: the unit tests
CARGO_TARGET_DIR=/Users/bytedance/dev/blaze-targets/u8 cargo +1.99.0 test -p ra-jit
# H, G: the new harness, against the fork's binaries and this branch's
RA_JIT_BIN=.../u1/release/ra-jit python3 harness/scriptrerun.py
RA_JIT_BIN=.../u8/release/ra-jit python3 harness/scriptrerun.py
```

The scripts and raw outputs of E1 to E4 are in `/Users/bytedance/dev/blaze-targets/u8-work` (`exp1.py`,
`e3.py`, `e1/`, `e3-before/`, `e3-after/`, `e4/`); they are not part of the repository.
