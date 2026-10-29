# Fixtures of `harness/dist/crossmachine.py`

Each directory is one repository, copied whole to a machine's checkout path.

- `shared/` a crate outside every workspace, reached as `path = "../../shared"` from `workspace/`.
- `workspace/` a workspace with a build script (`OUT_DIR` files included as code and as bytes), a
  path dependency outside it (`shared`), a registry dependency (`fastrand`, panicking inside its
  own source), `file!()`, panics with locations, and a function with the checkout's path in its
  data.
- `vendored/` a workspace whose `fastrand` is in `vendor/`, by source replacement, as
  `cargo vendor` lays it out.
- `macros/` a workspace with a proc-macro crate: two macros that read their input, and a derive
  that reads a file of the package it is used in while it expands. `ManifestLength` observes
  a machine path without emitting the path itself. It catches unsafe path normalization.

The harness generates `vendored/vendor/` from the locked registry archive in scratch space.
It is not a checked-in copy or an input taken from the implementer's ignored files.
The `vendor-edit` arm changes a vendored body and updates its checksum on B. B's
fed build must print the changed value, not A's cached value. The sibling fixture
also prints concatenated file paths with literal dot and repeated-slash suffixes.

Lock files are checked in, so no machine resolves anything; the registry crate comes from each
machine's own cargo home.
