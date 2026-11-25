//! Constant-masked IR grouping. Members retain their original symbols through their thunks.

use super::*;
use blaze_own::{CollectIn, FixedVec};

const MASK_DOMAIN: &str = "blaze-ladder fold mask 1";
const BODY_DOMAIN: &str = "blaze-ladder folded body 1";
const THUNK_DOMAIN: &str = "blaze-ladder folded thunk 1";
const GRANT_DOMAIN: &str = "blaze-ladder folded grant 1";
const MAX_PARAMETERS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Operand<R> {
    Int {
        bits: u8,
        value: u128,
    },
    F32(u32),
    F64(u64),
    Place(R),
    /// Member-local data by content, including the targets to which its words relocate.
    Data(ContentId),
}

pub type Constant = Operand<u32>;
type Cell = Operand<Reference>;

impl<R> Operand<R> {
    fn kind(&self) -> [u8; 2] {
        match self {
            Self::Int { bits, .. } => [0, *bits],
            Self::F32(_) => [1, 0],
            Self::F64(_) => [2, 0],
            Self::Place(_) => [3, 0],
            Self::Data(_) => [4, 0],
        }
    }
}

#[derive(Clone, Copy)]
pub struct Mask<'a> {
    /// Content of the prepared IR and signature with parameterizable constants erased.
    pub shape: ContentId,
    pub instructions: u32,
    pub constants: &'a [Constant],
}

pub struct Parameter {
    pub constant: usize,
    /// Equal columns of per-member values share one trailing argument.
    pub argument: usize,
}

pub struct Member<'a> {
    pub function: &'a Function<'a>,
    pub constants: &'a [Constant],
    pub thunk: CodeKey,
}

pub struct Fold<'a> {
    pub body: CodeKey,
    pub parameters: AVec<'a, Parameter>,
    /// Sorted by symbol. The first member provides the rewritten body's debug and local data.
    pub members: AVec<'a, Member<'a>>,
}

impl Fold<'_> {
    pub fn name(&self) -> GrantName {
        let mut hash = Hasher::new(GRANT_DOMAIN);
        hash.id(&self.body.0);
        count(&mut hash, self.members.len());
        for member in &self.members {
            hash.update(&member.function.symbol.0).id(&member.thunk.0);
        }
        GrantName(hash.finish())
    }
}

impl<R> Operand<R> {
    fn hash(self, hash: &mut Hasher, place: impl FnOnce(R, &mut Hasher)) {
        hash.update(&self.kind());
        match self {
            Self::Place(reference) => place(reference, hash),
            Self::Int { value, .. } => {
                hash.update(&value.to_le_bytes());
            }
            Self::F32(bits) => {
                hash.update(&bits.to_le_bytes());
            }
            Self::F64(bits) => {
                hash.update(&bits.to_le_bytes());
            }
            Self::Data(id) => {
                hash.id(&id);
            }
        };
    }
}

struct Candidate<'a> {
    function: &'a Function<'a>,
    mask: Mask<'a>,
    cells: AVec<'a, Cell>,
    recipe: CodeKey,
}

/// One part of the historical first-fit split. A class contains the constant positions whose
/// raw IR operands agreed in every member. A position varies when its resolved value differs.
struct Part<'a, 'b> {
    first: &'b Candidate<'a>,
    members: AVec<'a, &'b Candidate<'a>>,
    columns: AVec<'a, Column>,
}

struct Column {
    position: usize,
    first: Cell,
    varying: bool,
    class: usize,
    operand: Constant,
}

impl<'a, 'b> Part<'a, 'b> {
    fn new(first: &'b Candidate<'a>, alloc: Alloc<'a>) -> Self {
        let columns = first
            .cells
            .iter()
            .zip(first.mask.constants)
            .enumerate()
            .map(|(position, (&cell, &operand))| Column {
                position,
                first: cell,
                varying: false,
                class: 0,
                operand,
            })
            .collect_in(alloc);
        let mut part = Self { first, members: Vec::new_in(alloc), columns };
        part.add(first);
        part
    }

    fn fits(&self, next: &Candidate<'_>) -> bool {
        let mut representatives = FixedVec::<_, MAX_PARAMETERS>::new();
        for ((column, &cell), &operand) in
            self.columns.iter().zip(&next.cells).zip(next.mask.constants)
        {
            if column.varying || column.first != cell {
                let class = (column.class, operand);
                if !representatives.contains(&class) && representatives.push(class).is_err() {
                    return false;
                }
            }
        }
        true
    }

    fn add(&mut self, next: &'b Candidate<'a>) {
        for ((column, &cell), &operand) in
            self.columns.iter_mut().zip(&next.cells).zip(next.mask.constants)
        {
            column.varying |= column.first != cell;
            column.operand = operand;
        }
        self.columns.sort_unstable_by_key(|column| (column.class, column.operand));
        let mut previous = None;
        let mut class = 0;
        for column in &mut self.columns {
            let current = (column.class, column.operand);
            if previous.is_some_and(|last| last != current) {
                class += 1;
            }
            column.class = class;
            previous = Some(current);
        }
        self.columns.sort_unstable_by_key(|column| column.position);
        self.members.push(next);
    }

    fn group(self, bucket: ContentId, alloc: Alloc<'a>) -> Option<Fold<'a>> {
        if self.members.len() < 2 {
            return None;
        }
        let mut classes = Vec::new_in(alloc);
        let mut parameters = Vec::new_in(alloc);
        let mut body = Hasher::new(BODY_DOMAIN);
        body.id(&bucket).id(&self.first.recipe.0);
        for column in &self.columns {
            if !column.varying {
                body.update(&[0]);
                column.first.hash(&mut body, Reference::hash);
                continue;
            }
            let argument =
                classes.iter().position(|&other| other == column.class).unwrap_or_else(|| {
                    let argument = classes.len();
                    classes.push(column.class);
                    argument
                });
            parameters.push(Parameter { constant: column.position, argument });
            body.update(&[1]);
            count(&mut body, argument);
        }
        let n = self.members.len().widen_to::<u128>();
        let size = u128::from(self.first.mask.instructions);
        if classes.is_empty()
            || size * (n - 1) <= n * (2 + classes.len().widen_to::<u128>()) + size / 8
        {
            return None;
        }
        let body = CodeKey(body.finish());
        let members = self
            .members
            .into_iter()
            .map(|entry| {
                let mut thunk = Hasher::new(THUNK_DOMAIN);
                thunk.id(&body.0).id(&entry.recipe.0).update(&entry.function.symbol.0);
                count(&mut thunk, entry.mask.constants.len());
                for constant in entry.mask.constants {
                    constant.hash(&mut thunk, |place, hash| {
                        hash.update(&place.to_le_bytes());
                    });
                }
                Member {
                    function: entry.function,
                    constants: entry.mask.constants,
                    thunk: CodeKey(thunk.finish()),
                }
            })
            .collect_in(alloc);
        Some(Fold { body, parameters, members })
    }
}

pub(super) fn groups<'a>(
    functions: &ABTreeMap<'a, Symbol, &'a Function<'a>>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Fold<'a>>, PlanError> {
    let mut buckets = BTreeMap::new_in(alloc);
    for (&symbol, &function) in functions {
        let Backend::Foldable { recipe, mask } = function.backend else { continue };
        let mut shape = Hasher::new(MASK_DOMAIN);
        shape.id(&recipe.settings).id(&mask.shape).update(&mask.instructions.to_le_bytes());
        count(&mut shape, function.references.len());
        for reference in function.references {
            shape.update(&reference.kind());
        }
        count(&mut shape, mask.constants.len());
        let mut cells = Vec::with_capacity_in(mask.constants.len(), alloc);
        for &constant in mask.constants {
            shape.update(&constant.kind());
            let cell = match constant {
                Constant::Place(place) => {
                    let reference = function.references.get(place.widen_to::<usize>());
                    Cell::Place(
                        *reference.ok_or(PlanError::InvalidPlace { function: symbol, place })?,
                    )
                }
                Constant::Int { bits, value } => Cell::Int { bits, value },
                Constant::F32(bits) => Cell::F32(bits),
                Constant::F64(bits) => Cell::F64(bits),
                Constant::Data(id) => Cell::Data(id),
            };
            cells.push(cell);
        }
        let key = shape.finish();
        let bucket = buckets.get_or_insert_with(&key, |&key| (key, Vec::new_in(alloc)));
        bucket.push(Candidate { function, mask, cells, recipe: recipe_key(function, recipe) });
    }
    let mut groups = Vec::new_in(alloc);
    for (key, bucket) in buckets {
        if bucket.len() < 2 {
            continue;
        }
        let mut parts: AVec<'a, Part<'a, '_>> = Vec::new_in(alloc);
        for member in &bucket {
            if let Some(part) = parts.iter_mut().find(|part| part.fits(member)) {
                part.add(member);
            } else {
                parts.push(Part::new(member, alloc));
            }
        }
        groups.extend(parts.into_iter().filter_map(|part| part.group(key, alloc)));
    }
    Ok(groups)
}
