# u7-bound, run A3: what holds the memory as a governed cold tracedecay worker nears its bound

Profiles of run A3 (`--prof`, `lg_prof_sample:18`, one every 2 GB of live heap), folded with `heapfold.py` and grouped by `loadmem_holders.py`. MB sampled. The last is 14,563 MB live, shortly before the worker stopped itself at 16,387 MB footprint (docs/reports/LOADMEM.md, section 8).

## By category

| category | at-0007157 | at-0012092 | at-0014563 | gained |
|---|---|---|---|---|
| macro expansion (expansions, arguments, hygiene) | 1401 | 3255 | 2029 | 628 |
| bodies, signatures, item trees | 743 | 2006 | 1985 | 1242 |
| syntax (file parses, ast id maps) | 1029 | 1603 | 1547 | 518 |
| symbol table (rustc's symbols) | 1198 | 1198 | 1198 | 0 |
| MIR (built, runtime, monomorphized) | 0 | 0 | 1015 | 1015 |
| codegen products (compiled_instance, inlinable, roots) | 0 | 0 | 803 | 803 |
| ra-jit other | 760 | 760 | 784 | 24 |
| type inference and traits | 22 | 45 | 696 | 674 |
| name resolution (def maps, imports) | 285 | 550 | 669 | 384 |
| vfs and file loading | 450 | 450 | 450 | 0 |
| codegen driver outside queries (cached::Entries, StableNames, the object) | 0 | 0 | 441 | 441 |
| layouts | 0 | 0 | 223 | 223 |
| other salsa ingredients (interned) | 57 | 113 | 153 | 96 |
| other salsa ingredients (record) | 32 | 75 | 108 | 76 |
| other salsa ingredients (value) | 59 | 77 | 90 | 31 |
| other salsa ingredients (tracked_struct) | 3 | 8 | 11 | 8 |
| other salsa ingredients (other) | 0 | 2 | 5 | 5 |
| other salsa ingredients (input) | 1 | 1 | 1 | 0 |
| **sampled live, all holders** | 6046 | 10150 | 12214 | 6168 |

## By holder (top 40 at 14.6 GB live)

| holder | at-0014563 | gained |
|---|---|---|
| value <hir_expand::MacroCallId>::parse_macro_expansion::parse_macro_expansion_ | 1433 | 0 |
| value <base_db::editioned_file_id::EditionedFileId>::parse::parse_ | 1411 | 0 |
| own symbol table | 1198 | 0 |
| own ra-jit main (other) | 764 | 0 |
| value <hir_def::expr_store::body::Body>::with_source_map::with_source_map_ | 612 | 0 |
| value <hir_ty::infer::InferenceResult>::for_body::for_body_ | 513 | 0 |
| own vfs / file loading | 450 | 0 |
| own codegen driver (outside queries) | 441 | 0 |
| value <hir_def::expr_store::body::Body>::relative::relative_ | 395 | 0 |
| value core::result::Result<hir_ty_codegen::Produced, hir_ty_codegen::CodegenError>, <hir_ty_codegen::compiled_ | 390 | 0 |
| value hir_ty::mir::transform::runtime_mir_body_query | 348 | 0 |
| value hir_ty::mir::lower::mir_body_query | 264 | 0 |
| value hir_ty::mir::transform::runtime_mir_body_for_coroutine_query | 163 | 0 |
| record hir_def::expr_store::body::body_syntax | 156 | 0 |
| record hir_ty_codegen::compiled_instance | 153 | 0 |
| value hir_def::nameres::crate_local_def_map | 151 | 0 |
| value <hir_def::signatures::ImplSignature>::with_source_map::with_source_map_ | 141 | 0 |
| value <hir_expand::MacroCallId>::macro_arg::macro_arg_ | 139 | 0 |
| value hir_ty::mir::lower::mir_body_for_coroutine_query | 136 | 0 |
| value core::option::Option<hir_ty_codegen::fragment::Inlinable>, <hir_ty_codegen::inlinable | 135 | 0 |
| value <hir_expand::MacroCallId>::expand_proc_macro::expand_proc_macro_ | 117 | 0 |
| record <hir_def::nameres::ModuleScope>::name::name_ | 105 | 0 |
| record <hir_expand::MacroCallId>::parse_macro_expansion::parse_macro_expansion_ | 95 | 0 |
| interned span::hygiene::imp::SyntaxContextImpl | 85 | 0 |
| value hir_def::item_tree::file_item_tree_query | 84 | 0 |
| interned hir_expand::MacroCallId | 84 | 0 |
| value <hir_def::signatures::FunctionSignature>::with_source_map::with_source_map_ | 84 | 0 |
| interned _ | 81 | 0 |
| record <hir_def::expr_store::body::Body>::relative::relative_ | 79 | 0 |
| record <hir_expand::HirFileId>::ast_id_map::ast_id_map_ | 77 | 0 |
| **sampled live, all holders** | 12214 | 0 |
