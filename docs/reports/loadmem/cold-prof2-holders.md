# Holders of the profiled run (cold-prof2), MB sampled by jemalloc

Made by `harness/loadmem_holders.py` from the folded profiles at 3.9, 5.6, 13.1, 18.6 and 30.2 GB live. Raw profiles are not kept (150 MB).

## By build stage

| category | at-0003895 | at-0005607 | at-0013107 | at-0018622 | at-0030235 | gained |
|---|---|---|---|---|---|---|
| MIR (built, runtime, monomorphized) | 0 | 0 | 0 | 2473 | 6118 | 6118 |
| codegen products (compiled_instance, inlinable, roots) | 0 | 0 | 0 | 1679 | 4647 | 4647 |
| bodies, signatures, item trees | 0 | 444 | 2268 | 2158 | 2644 | 2644 |
| macro expansion (expansions, arguments, hygiene) | 0 | 866 | 3631 | 2049 | 2250 | 2250 |
| type inference and traits | 0 | 13 | 33 | 1207 | 2087 | 2087 |
| codegen driver outside queries (cached::Entries, StableNames, the object) | 0 | 0 | 0 | 676 | 1524 | 1524 |
| syntax (file parses, ast id maps) | 0 | 588 | 1614 | 1491 | 1512 | 1512 |
| symbol table (rustc's symbols) | 588 | 1186 | 1186 | 1186 | 1186 | 598 |
| layouts | 0 | 0 | 0 | 303 | 809 | 809 |
| ra-jit other | 2621 | 778 | 781 | 802 | 802 | -1819 |
| name resolution (def maps, imports) | 0 | 210 | 652 | 712 | 758 | 758 |
| vfs and file loading | 0 | 480 | 480 | 479 | 479 | 479 |
| other salsa ingredients (interned) | 0 | 44 | 140 | 168 | 211 | 211 |
| other salsa ingredients (record) | 0 | 21 | 71 | 119 | 165 | 165 |
| other salsa ingredients (value) | 0 | 52 | 81 | 99 | 112 | 112 |
| other salsa ingredients (other) | 0 | 1 | 2 | 8 | 21 | 21 |
| other salsa ingredients (tracked_struct) | 0 | 3 | 12 | 14 | 14 | 14 |
| other salsa ingredients (input) | 0 | 2 | 2 | 2 | 2 | 2 |
| **sampled live, all holders** | 3211 | 4698 | 10960 | 15634 | 25350 | 22139 |

## By driver on the stack

| driver | at-0005607 | at-0013107 | at-0018622 | at-0030235 | gained |
|---|---|---|---|---|---|
| compile_ahead (codegen, every core) | 0 | 0 | 6182 | 15071 | 15071 |
| other (front end run from rayon and salsa threads) | 2885 | 9018 | 7168 | 7994 | 5109 |
| symbol table load | 1306 | 1306 | 1306 | 1306 | 0 |
| load workspace (project model, vfs) | 486 | 486 | 484 | 484 | -2 |
| frozen / store | 8 | 8 | 269 | 269 | 261 |
| proc-macro expansion | 12 | 141 | 139 | 139 | 127 |
| find_stored_roots | 0 | 0 | 83 | 83 | 83 |
| **sampled live, all holders** | 4698 | 10960 | 15634 | 25350 | 20652 |

## By holder (top 40 at 30.2 GB)

| holder | at-0003895 | at-0005607 | at-0013107 | at-0018622 | at-0030235 | gained |
|---|---|---|---|---|---|---|
| value core::result::Result<hir_ty_codegen::Produced, hir_ty_codegen::CodegenError>, <hir_ty_codegen::compiled_ | 0 | 0 | 0 | 771 | 2195 | 2195 |
| value <hir_expand::MacroCallId>::parse_macro_expansion::parse_macro_expansion_ | 0 | 718 | 3049 | 1434 | 1619 | 1619 |
| value hir_ty::mir::transform::monomorphized_runtime_mir_body_for_coroutine_query | 0 | 0 | 0 | 406 | 1525 | 1525 |
| value hir_ty::mir::transform::runtime_mir_body_query | 0 | 0 | 0 | 849 | 1524 | 1524 |
| own codegen driver (outside queries) | 0 | 0 | 0 | 676 | 1524 | 1524 |
| value <hir_ty::infer::InferenceResult>::for_body::for_body_ | 0 | 0 | 0 | 921 | 1492 | 1492 |
| value <base_db::editioned_file_id::EditionedFileId>::parse::parse_ | 0 | 546 | 1464 | 1360 | 1368 | 1368 |
| value hir_ty::mir::lower::mir_body_query | 0 | 0 | 0 | 672 | 1212 | 1212 |
| own symbol table | 588 | 1186 | 1186 | 1186 | 1186 | 598 |
| value <hir_def::expr_store::body::Body>::with_source_map::with_source_map_ | 0 | 69 | 951 | 736 | 956 | 956 |
| record hir_ty_codegen::compiled_instance | 0 | 0 | 0 | 358 | 849 | 849 |
| own ra-jit main (other) | 2621 | 774 | 777 | 783 | 783 | -1838 |
| record hir_ty_codegen::inlinable | 0 | 0 | 0 | 249 | 735 | 735 |
| value hir_ty::mir::transform::runtime_mir_body_for_coroutine_query | 0 | 0 | 0 | 196 | 719 | 719 |
| value hir_ty::mir::lower::mir_body_for_coroutine_query | 0 | 0 | 0 | 157 | 604 | 604 |
| value core::option::Option<hir_ty_codegen::fragment::Inlinable>, <hir_ty_codegen::inlinable | 0 | 0 | 0 | 193 | 569 | 569 |
| value <hir_def::expr_store::body::Body>::relative::relative_ | 0 | 145 | 548 | 387 | 539 | 539 |
| own vfs / file loading | 0 | 480 | 480 | 479 | 479 | 479 |
| record <hir_ty::infer::InferenceResult>::for_body::for_body_ | 0 | 0 | 0 | 90 | 208 | 208 |
| value hir_ty::layout::layout_of_ty_query | 0 | 0 | 0 | 72 | 196 | 196 |
| record hir_def::expr_store::body::body_syntax | 0 | 28 | 132 | 154 | 161 | 161 |
| value hir_ty::layout::adt::layout_of_adt_query | 0 | 0 | 0 | 54 | 160 | 160 |
| value <hir_expand::MacroCallId>::macro_arg::macro_arg_ | 0 | 52 | 152 | 160 | 160 | 160 |
| value hir_def::nameres::crate_local_def_map | 0 | 92 | 153 | 154 | 154 | 154 |
| record <hir_def::nameres::ModuleScope>::name::name_ | 0 | 19 | 106 | 124 | 141 | 141 |
| record hir_ty::method_resolution::lookup_impl_method_stored | 0 | 0 | 0 | 54 | 139 | 139 |
| value <hir_def::signatures::ImplSignature>::with_source_map::with_source_map_ | 0 | 38 | 136 | 137 | 137 | 137 |
| value alloc::vec::Vec<ra_ap_rustc_abi::VariantLayout<hir_ty::layout::RustcFieldIdx>> as alloc::vec::spec_from_ | 0 | 0 | 0 | 57 | 129 | 129 |
| interned _ | 0 | 16 | 67 | 89 | 126 | 126 |
| value <hir_expand::MacroCallId>::expand_proc_macro::expand_proc_macro_ | 0 | 10 | 117 | 116 | 116 | 116 |
| value core::result::Result<hir_ty_codegen::Produced, hir_ty_codegen::CodegenError>, <hir_ty_codegen::compiled_ | 0 | 0 | 0 | 31 | 112 | 112 |
| record <hir_expand::MacroCallId>::parse_macro_expansion::parse_macro_expansion_ | 0 | 20 | 82 | 97 | 109 | 109 |
| record hir_ty::layout::layout_of_ty_query | 0 | 0 | 0 | 39 | 109 | 109 |
| record <hir_def::expr_store::body::Body>::with_source_map::with_source_map_ | 0 | 6 | 76 | 95 | 109 | 109 |
| value <hir_def::signatures::FunctionSignature>::with_source_map::with_source_map_ | 0 | 8 | 8 | 88 | 104 | 104 |
| value hir_ty::mir::transform::runtime_mir_body_for_closure_query | 0 | 0 | 0 | 23 | 98 | 98 |
| value hir_def::item_tree::file_item_tree_query | 0 | 41 | 96 | 97 | 97 | 97 |
| record <hir_def::expr_store::body::Body>::relative::relative_ | 0 | 22 | 60 | 78 | 94 | 94 |
| interned span::hygiene::imp::SyntaxContextImpl | 0 | 20 | 84 | 89 | 89 | 89 |
| value hir_ty::method_resolution::lookup_impl_method_stored | 0 | 0 | 0 | 35 | 87 | 87 |
| **sampled live, all holders** | 3211 | 4698 | 10960 | 15634 | 25350 | 22139 |
