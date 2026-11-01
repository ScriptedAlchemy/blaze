//! The code generator's hot paths: the key a function's entry is kept under in the store.
//!
//! Every benchmark does the same work on every run, so that tango can pair the binary of a
//! change with the binary of its base.

use std::hint::black_box;

use hir_ty_codegen::bench::entry_key;
use ra_jit_store::bench::key_of;
use tango_bench::{IntoBenchmarks, benchmark_fn, tango_benchmarks};

/// A name as long as a mangled generic function's.
const NAME: &str = "_ZN4core4iter6traits8iterator8Iterator4fold17h0123456789abcdefE\
                    <alloc::vec::into_iter::IntoIter<u32>, usize, closure>";

fn benchmarks() -> impl IntoBenchmarks {
    [benchmark_fn("key/entry", |b| {
        let program = key_of(7);
        b.iter(move || entry_key(black_box(NAME), false, &program))
    })]
}

tango_benchmarks!(benchmarks());
