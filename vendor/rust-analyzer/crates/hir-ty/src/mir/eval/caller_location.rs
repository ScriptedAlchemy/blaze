//! `caller_location` follows rustc's per-frame `track_caller_arg`, and constructs Location
//! using its target layout, as `rustc_const_eval::util::caller_location` does.
//! https://github.com/rust-lang/rust/blob/main/compiler/rustc_const_eval/src/util/caller_location.rs

use hir_def::src::HasSource;
use syntax::AstNode;

use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) enum CallerLocation<'db> {
    Call(InferBodyId<'db>, MirSpan),
    Declaration(FunctionId),
}

impl<'db> Evaluator<'db> {
    pub(super) fn caller_location(
        &mut self,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, Address> {
        let site = locals.caller_location.unwrap_or(CallerLocation::Call(locals.body.owner, span));
        let (file, line, column) = match site {
            CallerLocation::Call(owner, span) => {
                let (file, range) = super::super::location::source_range(self.db, owner, span)
                    .ok_or_else(|| {
                        MirEvalError::NotSupported("caller location without source".into())
                    })?;
                super::super::location::caller_location_of(self.db, file, range)
            }
            CallerLocation::Declaration(function) => {
                let source = function.lookup(self.db).source(self.db);
                super::super::location::caller_location_of(
                    self.db,
                    source.file_id,
                    super::super::location::written_range(source.value.syntax()),
                )
            }
        }
        .ok_or_else(|| MirEvalError::NotSupported("caller location without source".into()))?;
        let file = super::super::location::source_name(self.db, file).ok_or_else(|| {
            MirEvalError::NotSupported("caller location without a source name".into())
        })?;
        let key = (file.clone(), line, column);
        if let Some(&address) = self.caller_locations.get(&key) {
            return Ok(address);
        }
        if file.as_bytes().contains(&0) {
            not_supported!("caller location with a NUL in its source name");
        }
        let Some(location) = self.lang_items().PanicLocation else {
            not_supported!("core::panic::Location is unavailable");
        };
        let args = GenericArgs::new_from_slice(&[Region::new_static(self.interner()).into()]);
        let layout = self.layout_adt(location.into(), args)?;
        // rustc interns both the location and its NUL-terminated filename. Repeated
        // calls at one source position must retain their pointer identity.
        let name = if let Some(&name) = self.caller_location_files.get(&file) {
            name
        } else {
            let name = self.heap_allocate(file.len() + 1, 1)?;
            let mut text = file.as_bytes().to_vec();
            text.push(0);
            self.write_memory(name, &text)?;
            self.caller_location_files.insert(file.clone(), name);
            name
        };
        let mut filename = name.to_bytes()[..self.ptr_size()].to_vec();
        filename.extend_from_slice(&file.len().to_le_bytes()[..self.ptr_size()]);
        let bytes = self.construct_with_layout(
            layout.size.bytes_usize(),
            &layout,
            None,
            [filename, line.to_le_bytes().to_vec(), column.to_le_bytes().to_vec()]
                .into_iter()
                .map(IntervalOrOwned::Owned),
        )?;
        let address =
            self.heap_allocate(layout.size.bytes_usize(), layout.align.abi.bytes_usize())?;
        self.write_memory(address, &bytes)?;
        self.caller_locations.insert(key, address);
        Ok(address)
    }
}
