//! Feed the actual release import closure or the generalized body and its thunks into LLVM.

use super::Native;
use crate::backend_plan::{
    Fold, Function,
    execute::{Emit, Emitted, Output, OutputKind},
};
use blaze_code::record::Symbol;
use blaze_emit::{
    Isa,
    llvm::{LlvmModule, LowerError, emit, folding},
};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine, Os};

#[derive(Clone, Copy)]
pub struct Compiler {
    machine: Machine,
}

impl Compiler {
    pub fn new(isa: Isa) -> Self {
        let os = match isa.os {
            blaze_emit::Os::MacOs => Os::MacOs,
            blaze_emit::Os::Linux => Os::Linux,
        };
        Self {
            machine: Machine {
                arch: Arch::Arm64,
                os,
                features: if isa.lse { Machine::LSE } else { 0 },
            },
        }
    }

    pub fn identity(self, builder: &crate::code_records::Builder) -> ContentId {
        Hasher::new("blaze-ladder native backend settings 1")
            .id(&emit::identity(self.machine, RELEASE_OPT))
            .id(builder.producer.id())
            .part(&builder.toolchain.0)
            .finish()
    }
}

const RELEASE_OPT: u8 = 3;

pub struct Renderer<'s, 'a, 'r> {
    pub catalog: &'s Native<'a, 'r>,
    pub compiler: Compiler,
}

#[derive(Debug)]
pub enum Error {
    Missing(Symbol),
    UnexpectedStandalone(Symbol),
    MissingBody,
    Kind { output: Symbol, wanted: OutputKind },
    Lower(LowerError),
    Llvm { symbol: Symbol, why: emit::EmitError },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing(s) => write!(f, "no grounded LLVM module for {s}"),
            Self::UnexpectedStandalone(s) => {
                write!(f, "native release work for {s} requires a release or folded grant")
            }
            Self::MissingBody => {
                f.write_str("the folded grant has no member from which to build its shared body")
            }
            Self::Kind { output, wanted } => {
                write!(f, "LLVM returned a different output kind for {output}, expected {wanted:?}")
            }
            Self::Lower(why) => core::fmt::Display::fmt(why, f),
            Self::Llvm { symbol, why } => write!(f, "emitting {symbol}: {why}"),
        }
    }
}

impl<'a, 'r> Renderer<'_, 'a, 'r> {
    fn module(&self, symbol: Symbol) -> Result<&'r LlvmModule<'a>, Error> {
        self.catalog.module(symbol).map(|body| &body.module).ok_or(Error::Missing(symbol))
    }
}

fn emitted(made: emit::Emission<'_>, kind: OutputKind) -> Result<Emitted<'_>, Error> {
    if !matches!(
        (made.backend, kind),
        (emit::Backend::ReleaseBlob, OutputKind::ReleaseBlob)
            | (emit::Backend::FoldedOutput, OutputKind::FoldedBody | OutputKind::Thunk)
    ) {
        return Err(Error::Kind { output: made.symbol, wanted: kind });
    }
    Ok(Emitted { output: Output { symbol: made.symbol, kind }, record: made.record })
}

impl Emit for Renderer<'_, '_, '_> {
    type Error = Error;

    fn standalone<'a>(
        &mut self,
        function: &Function<'_>,
        _: Alloc<'a>,
    ) -> Result<Emitted<'a>, Error> {
        Err(Error::UnexpectedStandalone(function.symbol))
    }

    fn release<'a>(
        &mut self,
        function: &Function<'_>,
        imports: &[&Function<'_>],
        alloc: Alloc<'a>,
    ) -> Result<Emitted<'a>, Error> {
        let mut modules = Vec::with_capacity_in(imports.len(), alloc);
        for import in imports {
            modules.push(self.module(import.symbol)?);
        }
        let task = emit::ReleaseTask {
            module: self.module(function.symbol)?,
            imports: &modules,
            opt: RELEASE_OPT,
            machine: self.compiler.machine,
        };
        emitted(
            emit::emit_release(task, alloc)
                .map_err(|why| Error::Llvm { symbol: function.symbol, why })?,
            OutputKind::ReleaseBlob,
        )
    }

    fn fold<'a>(
        &mut self,
        group: &Fold<'_>,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, Emitted<'a>>, Error> {
        let first = group.members.first().ok_or(Error::MissingBody)?;
        let body = Symbol(group.body.0.0);
        let columns = || group.parameters.iter().map(|p| (p.constant, p.argument));
        let generalized = folding::module(
            self.module(first.function.symbol)?,
            body,
            folding::Role::Body,
            columns(),
            alloc,
        )
        .map_err(Error::Lower)?;
        let mut thunks = Vec::with_capacity_in(group.members.len(), alloc);
        for member in &group.members {
            thunks.push(
                folding::module(
                    self.module(member.function.symbol)?,
                    body,
                    folding::Role::Thunk,
                    columns(),
                    alloc,
                )
                .map_err(Error::Lower)?,
            );
        }
        let mut members = Vec::with_capacity_in(thunks.len() + 1, alloc);
        members.push(emit::FoldMember { module: &generalized, is_body: true });
        for module in &thunks {
            members.push(emit::FoldMember { module, is_body: false });
        }
        let made = emit::emit_fold(
            emit::FoldTask { members: &members, machine: self.compiler.machine },
            alloc,
        )
        .map_err(|why| Error::Llvm { symbol: body, why })?;
        let mut rows = Vec::with_capacity_in(made.thunks.len() + 1, alloc);
        rows.push(emitted(made.body, OutputKind::FoldedBody)?);
        for thunk in made.thunks {
            rows.push(emitted(thunk, OutputKind::Thunk)?);
        }
        Ok(rows)
    }
}
