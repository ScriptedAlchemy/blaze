# Native request environment

The local daemon request carries a complete client environment snapshot.
Keys and values are `OsString`, not lossy text. An absent variable stays
absent; an empty value stays empty. Source queries get a separate Unicode
projection. Build scripts and linker commands get native values.

The snapshot schema, version 1, is deterministic CBOR:

```text
[1, platform, [[key_bytes, value_bytes], ...]]
```

`platform` is 0 for Unix bytes and 1 for Windows little-endian UTF-16.
Pairs are ordered by their native keys and duplicate keys are refused.
The decoder refuses another platform, invalid environment names and
noncanonical CBOR. Windows preserves unpaired UTF-16 code units.

The existing local line protocol frames this CBOR as lowercase hex inside
its request envelope. A worker environment line is
`native-env-v1:[hex_snapshot,build_number]`, with the outer pair framed as
JSON for compatibility with the existing socket. Existing Unicode-only
worker clients remain readable. This does not convert the older daemon
envelope or command protocol to CBOR; that migration remains separate.

Worker loads receive the same snapshot over stdin, before any other reader
starts. Lazy load readers use an in-memory snapshot. A spare receives
it before loading the workspace. Warm requests keep their native snapshot
through script admission and command construction. Detached link commands
capture the environment before leaving that operation. Warm workers never
change process-global variables for a request.

Environment debug output lists names only. Malformed environment diagnostics
do not echo request bytes. No environment value is written to a diagnostic.
No load-environment file is written. Legacy build-script run records keep
names only; replay snapshots live in daemon memory and are lost on restart.
Owned script freshness records store HMAC-SHA256 identities of values actually
read, using a random private key in the daemon home. Private owned input files
store keyed identities. Cargo's fingerprint of a run the daemon makes in
Cargo's `OUT_DIR` is Cargo's file, written as Cargo 1.99 writes it: it has the
value of each variable the script declared with `rerun-if-env-changed`, and of
no other, because Cargo reads those values to decide its next run
(`LocalFingerprint::from_env`). It is byte for byte the fingerprint Cargo
1.99.0 wrote for the same run (`script.rs`'s
`a_run_the_daemon_makes_is_recorded_byte_for_byte_as_cargo_records_it`, against
Cargo's own file). The private file Cargo's leg is given of such a run holds
keyed identities, not that fingerprint, and neither has the value of a
variable the script read without declaring it
(`a_completed_run_carries_no_value_of_a_variable_to_cargos_leg`). No command of
the CLI reaches this leg at this head: whole builds and checks are blaze's own,
and other builds are cargo's, so these runs are exercised by unit tests only.
Cargo-facing dep-info follows
Cargo's literal format and records only variables source actually read, not
the caller snapshot.
The key is not an environment value and is never part of a shared store.
Legacy compiler-unit, wrapper-state and doctest replay records also retain
environment field names only. Their values travel over the daemon socket and
are restored from daemon memory; losing that daemon loses replay eligibility.
Owned JSON project descriptions send per-crate values over stdin. The on-disk
description lists names and an empty environment, not captured caller values.
The strict macro witness plants its token in the product environment only and
scans the target, producer directories, store and live daemon home on each row,
then again before and after stopping the daemon, before deleting the home.
