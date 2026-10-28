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

Worker loads receive the same snapshot in a private file. A spare receives
it before loading the workspace. Warm requests keep their native snapshot
through script admission and command construction. Detached link commands
capture the environment before leaving that operation. Warm workers never
change process-global variables for a request.

Environment debug output lists names only. Malformed environment diagnostics
do not echo request bytes. No environment value is written to a diagnostic.
The private load file and wire are transport, not logs.
