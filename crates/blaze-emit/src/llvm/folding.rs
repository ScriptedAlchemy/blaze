//! The generalized LLVM definition and an ABI-preserving wrapper for each original symbol.

use super::{LinkName, LlvmModule, Lower, LowerError, Signature};
use blaze_code::record::{Space, Symbol, Target};
use blaze_lir::{
    Op, Scalar,
    func::{FnAttrs, SigId, SigRec},
    ty::Form,
};
use blaze_own::{AString, Alloc, Vec, format_in, format_into};

pub struct Abi<'a> {
    end_parameters: usize,
    end_header: usize,
    result: AString<'a>,
    noreturn: bool,
}

impl<'a> Abi<'a> {
    pub(super) fn of(
        lower: &Lower<'a, '_>,
        signature: Signature<'_>,
        end_parameters: usize,
        end_header: usize,
    ) -> Option<Self> {
        let view = lower.view;
        // A variadic tail, a musttail obligation, or an unwind/position site belongs to the
        // original frame. A member-local datum may escape and preserve its address.
        let own = Target { space: Space::Func, symbol: lower.symbol };
        if lower.literals.is_empty()
            || !view.datas.is_empty()
            || lower.references.contains(&Some(own))
            || view.sig(SigId(view.head.sig)).bits & SigRec::VARIADIC != 0
            || view
                .block_kinds
                .iter()
                .filter_map(|&kind| blaze_lir::BlockKind::from_byte(kind))
                .any(|kind| kind.is_landing())
            || (0..view.len()).any(|at| {
                matches!(
                    view.op(at),
                    Op::TailCall | Op::TailCallIndirect | Op::Invoke | Op::InvokeIndirect
                )
            })
            || lower.literals.iter().any(|literal| {
                literal.ty.form() != Form::Scalar
                    || matches!(literal.ty.elem(), Scalar::None | Scalar::Ptr)
            })
        {
            return None;
        }
        Some(Self {
            end_parameters,
            end_header,
            result: format_in!(lower.alloc, "{}", signature.result()),
            noreturn: view.head.attrs & FnAttrs::NORETURN != 0,
        })
    }
}

#[derive(Clone, Copy)]
pub enum Role {
    Body,
    Thunk,
}

/// Each pair names a literal ordinal and its trailing argument. Equal columns of values
/// coalesce into one argument. Floats pass their integer bits to the original bitcast.
pub fn module<'a>(
    original: &LlvmModule<'_>,
    body: Symbol,
    role: Role,
    parameters: impl Iterator<Item = (usize, usize)>,
    alloc: Alloc<'a>,
) -> Result<LlvmModule<'a>, LowerError> {
    let bad = |reason| LowerError { symbol: original.symbol, instruction: None, reason };
    if !original.traps.is_empty() || !original.positions.is_empty() {
        return Err(bad("folded member has source-specific machine positions"));
    }
    let abi =
        original.fold_abi.as_ref().ok_or_else(|| bad("folded member has an unforwardable ABI"))?;
    let ir = core::str::from_utf8(&original.ir)
        .map_err(|_| bad("prepared LLVM assembly is not UTF-8"))?;
    let mut columns = Vec::with_capacity_in(original.literals.len(), alloc);
    columns.resize(original.literals.len(), None);
    let mut args = Vec::new_in(alloc);
    for (literal, argument) in parameters {
        let column =
            columns.get_mut(literal).ok_or_else(|| bad("fold parameter outside literal table"))?;
        if column.replace(argument).is_some() {
            return Err(bad("fold literal names two arguments"));
        }
        let value = original
            .literals
            .get(literal)
            .ok_or_else(|| bad("fold parameter without a literal"))?;
        let raw = (value.ty.elem().bits(64), value.bits);
        if args.len() == argument {
            args.push(raw);
        } else if args.get(argument) != Some(&raw) {
            return Err(bad("fold argument has a gap or merges unequal literal operands"));
        }
    }
    if args.is_empty() {
        return Err(bad("folded body has no varying argument"));
    }
    let range = |a, b| ir.get(a..b).ok_or_else(|| bad("invalid prepared LLVM operand boundary"));
    let name = &original.definition_name;
    let prefix = range("define ".len(), name.start)?;
    let end =
        abi.end_parameters.checked_sub(1).ok_or_else(|| bad("empty folded parameter range"))?;
    let args_start = name
        .end
        .checked_add(1)
        .ok_or_else(|| bad("folded parameter range exceeds address space"))?;
    let forwarded = range(args_start, end)?;
    let target = Target { space: Space::Func, symbol: body };
    let body_name = format_in!(alloc, "@{}", LinkName(target));
    let mut rewritten = AString::new_in(alloc);
    let mut references = Vec::new_in(alloc);
    let mut implicit_references = Vec::new_in(alloc);
    let mut calls = Vec::new_in(alloc);
    let (symbol, definition_name) = match role {
        Role::Body => {
            rewritten.push_str(range(0, name.start)?);
            let start = rewritten.len();
            rewritten.push_str(body_name.as_str());
            let name_range = start..rewritten.len();
            rewritten.push_str(range(name.end, end)?);
            append_arguments(&mut rewritten, forwarded, &args, Args::Named);
            let mut after = end;
            for (literal, column) in original.literals.iter().zip(&columns) {
                if let Some(argument) = column {
                    rewritten.push_str(range(after, literal.operand.start)?);
                    format_into!(&mut rewritten, "%fold{argument}");
                    after = literal.operand.end;
                }
            }
            rewritten.push_str(range(after, ir.len())?);
            references.extend_from_slice(&original.references);
            implicit_references.extend_from_slice(&original.implicit_references);
            calls.extend_from_slice(&original.calls);
            (body, name_range)
        }
        Role::Thunk => {
            rewritten.push_str(range(0, abi.end_header)?);
            rewritten.push_str("entry:\n  ");
            let returns_value = abi.result.as_str() != "void";
            if returns_value {
                rewritten.push_str("%result = ");
            }
            format_into!(&mut rewritten, "tail call {prefix}{body_name}({forwarded}");
            append_arguments(&mut rewritten, forwarded, &args, Args::Values);
            rewritten.push_str(")\n  ");
            if abi.noreturn {
                rewritten.push_str("unreachable");
            } else if returns_value {
                format_into!(&mut rewritten, "ret {} %result", abi.result);
            } else {
                rewritten.push_str("ret void");
            }
            format_into!(&mut rewritten, "\n}}\ndeclare {prefix}{body_name}({forwarded}");
            append_arguments(&mut rewritten, forwarded, &args, Args::Types);
            rewritten.push_str(")\n");
            references.push(Some(target));
            calls.push(body);
            (original.symbol, name.start..name.end)
        }
    };
    Ok(LlvmModule {
        symbol,
        ir: rewritten.into_bytes(),
        definition_name,
        fold_abi: None,
        references,
        implicit_references,
        calls,
        literals: Vec::new_in(alloc),
        positions: Vec::new_in(alloc),
        traps: Vec::new_in(alloc),
        instructions: original.instructions,
    })
}

#[derive(Clone, Copy)]
enum Args {
    Named,
    Values,
    Types,
}

fn append_arguments(out: &mut AString<'_>, forwarded: &str, args: &[(u32, u128)], form: Args) {
    for (argument, &(width, bits)) in args.iter().enumerate() {
        if !forwarded.is_empty() || argument != 0 {
            out.push_str(", ");
        }
        format_into!(out, "i{width}");
        match form {
            Args::Named => format_into!(out, " %fold{argument}"),
            Args::Values => format_into!(out, " {bits}"),
            Args::Types => {}
        }
    }
}

#[cfg(test)]
mod tests;
