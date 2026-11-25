---
status: plan
summary: "Explicit cdylib selection and entry-free Rust code-record exports for native loaders."
date: 2026-11-25
---
# Shared-library export

`blaze export -p exported --lib -o libexported.dylib` selects exactly the named package's
`cdylib` target. The existing binary selection stays unchanged; exporting an app does not
implicitly export unrelated workspace libraries. The manifest layout retains crate types.

The product is `blaze_export::Entry::Cdylib`, with no entry symbol. `ProgramIn::exports`
contains the selected crate's exported C names and instance symbols. These are reachability
roots even without a native C image. The maker's library start does not look for `main` or
instantiate `lang_start`. Personality, allocation and TLS dependencies use the existing walk.

Mach-O and ELF writers consume the same laid code/data records and emit actual loader-facing
exports. Mach-O uses `MH_DYLIB`, an install name, export trie and signing; ELF omits executable
startup and PIE/interpreter requirements and supplies defined dynamic symbols. TLS is initialized
as a library. Product kind and the named exports participate in output identity; item code records
remain reusable where their semantic inputs agree. Distributed compilation uses the same exported
root set, with no fabricated executable entry or implicit local fallback.

Proof is the allocating `cdylib-allocates` fixture, exported explicitly then loaded by its app,
on macOS and Linux arm64; it prints `12` on cold and restarted warm runs. Compare library bytes
across replay and change source/settings to verify identity changes. Preserve executable behavior
and verify symbols, native callbacks, TLS initialization and destruction through the OS loader.
