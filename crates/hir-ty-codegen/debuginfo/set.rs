//! The sets of types a debugger is told of, kept so that the sets with a type share what it
//! is told as. Every type a variable's type is made of is in its set, and every set of a type
//! with a `String` in it has the `String`, its `Vec`, its `RawVec` and the rest: a set is a
//! type ([`Node`]) with the sets of the types it is made of, which the sets with each of
//! those share. What a type is told as apart from the types it is made of ([`Told`]) is
//! interned.
//!
//! The database keeps what each type is told as on its own, with the types it is made of by
//! their keys ([`Own`]), and [`resolve`] puts the sets together from that. Types made of each
//! other, as a struct that holds a `Vec` of its own kind is with the `Vec` through the
//! `Vec`'s generic parameter, are put together as one group, in which each names the others
//! by their places ([`Part::Here`]); every group is put together once, for every set it is
//! in. The store keeps a set as a list of [`DebugType`]s that name each other by their
//! places in it ([`TypeSet::to_vec`]).

use std::{fmt, hash::Hash, sync::Arc};

use intern::{Interned, Symbol, impl_internable};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{Aggregate, DebugType, Field, Tag, TypeIndex, Variant};

/// A type and every type it is made of: a type of a group of types made of each other, and
/// of the sets of other types. The sets with a type share its set.
#[derive(Clone)]
pub(crate) struct TypeSet {
    group: Arc<[Node]>,
    nth: u32,
}

pub(super) struct Node {
    pub(super) told: Interned<Told>,
    /// The types it is made of, in the order [`Told`] says.
    of: Box<[Part]>,
}

/// A type that a type is made of.
#[derive(Clone)]
enum Part {
    Type(TypeSet),
    /// The type at this place in the group of the one made of it.
    Here(u32),
    /// What a pointer points to past the pointers followed from a variable: the type of this
    /// name, where a debugger is told of one.
    Named(Symbol),
}

/// A type that a type is made of, as [`TypeSet::of`] tells it.
#[derive(Clone)]
pub(super) enum Of<'a> {
    Type(TypeSet),
    /// See [`Part::Named`].
    Named(&'a Symbol),
}

/// What one type is told as, with the types it is made of by their keys: what the database
/// keeps of each type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Own<K> {
    pub(super) told: Interned<Told>,
    pub(super) of: Box<[Made<K>]>,
}

/// A type that one of an [`Own`] is made of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Made<K> {
    Key(K),
    /// See [`Part::Named`].
    Named(Symbol),
    /// A type that is no type of the program's, as the vtable of a pointer to a `dyn` is.
    Inline(Box<Own<K>>),
}

/// Where [`resolve`] finds the types of a set, by a key each.
pub(super) trait Found {
    type Key: Clone + Eq + Hash;
    /// What the type of `key` is told as, `None` for one a debugger is told nothing of.
    fn own(&mut self, key: &Self::Key) -> Option<Arc<Own<Self::Key>>>;
    /// The set of `key` put together before, `Some(None)` for a type told nothing of.
    fn set(&mut self, key: &Self::Key) -> Option<Option<TypeSet>>;
    /// Keeps the set of `key` for every set it is in.
    fn keep(&mut self, key: &Self::Key, set: Option<TypeSet>);
}

/// The set of the type of `key`, as `found` has the types.
pub(super) fn resolve<F: Found>(found: &mut F, key: &F::Key) -> Option<TypeSet> {
    let mut search = Search { found, on: FxHashMap::default(), stack: Vec::new() };
    match search.key(key, 0).0 {
        Seen::Done(Some(Part::Type(set))) => Some(set),
        _ => None,
    }
}

/// What [`Search`] found of a type that a type is made of.
enum Seen {
    /// `None` for a type a debugger is told nothing of.
    Done(Option<Part>),
    /// The type at this place on the stack, which is being put together.
    At(usize),
}

/// The types being put together, by the groups of types made of each other that they are
/// in, as Tarjan's algorithm finds them.
struct Search<'a, F: Found> {
    found: &'a mut F,
    /// The place on `stack` of each type there.
    on: FxHashMap<F::Key, usize>,
    stack: Vec<Pending<F::Key>>,
}

struct Pending<K> {
    /// `None` for one of [`Made::Inline`].
    key: Option<K>,
    told: Interned<Told>,
    of: Vec<Seen>,
    /// The lowest place on the stack of a type it is made of, through the types it is made of.
    low: usize,
}

impl<F: Found> Search<'_, F> {
    /// What the type of `key` is, as a part of a type `depth` types inside a variable's, and
    /// the lowest place on the stack it is made of. A type inside more than
    /// [`DebugType::DEEPEST`] others is not told of, as the writer tells of none: a list read
    /// from the store may be made of any number.
    fn key(&mut self, key: &F::Key, depth: u32) -> (Seen, usize) {
        if let Some(&at) = self.on.get(key) {
            return (Seen::At(at), at);
        }
        if let Some(set) = self.found.set(key) {
            return (Seen::Done(set.map(Part::Type)), usize::MAX);
        }
        if depth >= DebugType::DEEPEST {
            return (Seen::Done(None), usize::MAX);
        }
        let Some(own) = self.found.own(key) else {
            self.found.keep(key, None);
            return (Seen::Done(None), usize::MAX);
        };
        self.on.insert(key.clone(), self.stack.len());
        self.own(Some(key.clone()), &own, depth)
    }

    fn own(&mut self, key: Option<F::Key>, own: &Own<F::Key>, depth: u32) -> (Seen, usize) {
        let at = self.stack.len();
        let of = Vec::with_capacity(own.of.len());
        self.stack.push(Pending { key, told: own.told.clone(), of, low: at });
        for made in &own.of {
            let (seen, low) = match made {
                Made::Key(key) => self.key(key, depth + 1),
                Made::Named(name) => (Seen::Done(Some(Part::Named(name.clone()))), usize::MAX),
                Made::Inline(own) => self.own(None, own, depth + 1),
            };
            let pending = &mut self.stack[at];
            pending.low = pending.low.min(low);
            pending.of.push(seen);
        }
        let low = self.stack[at].low;
        if low < at {
            return (Seen::At(at), low);
        }
        // The type and the ones after it on the stack are made of each other, and of no type
        // before it.
        let group: Vec<Pending<F::Key>> = self.stack.drain(at..).collect();
        for key in group.iter().filter_map(|it| it.key.as_ref()) {
            self.on.remove(key);
        }
        // An array of a type that is not told of is not told of either, and is a group of its
        // own: it is made of nothing else.
        if let [array] = &group[..]
            && let Told::Array { .. } = &*array.told
            && !matches!(array.of.first(), Some(Seen::Done(Some(Part::Type(_)))))
        {
            if let Some(key) = &array.key {
                self.found.keep(key, None);
            }
            return (Seen::Done(None), usize::MAX);
        }
        let nodes: Arc<[Node]> = group
            .iter()
            .map(|it| {
                let of = it.of.iter().map(|seen| match seen {
                    Seen::At(on) => Some(Part::Here((on - at) as u32)),
                    Seen::Done(part) => part.clone(),
                });
                node(&it.told, of.collect())
            })
            .collect();
        for (nth, it) in group.iter().enumerate() {
            if let Some(key) = &it.key {
                let set = TypeSet { group: nodes.clone(), nth: nth as u32 };
                self.found.keep(key, Some(set));
            }
        }
        (Seen::Done(Some(Part::Type(TypeSet { group: nodes, nth: 0 }))), usize::MAX)
    }
}

/// `told`, made of the types `of`: without the fields and the parameters of a type that is
/// not told of (`None`), nor what a pointer points to then, as the list the store keeps has
/// it ([`expand`]).
fn node(told: &Interned<Told>, of: Vec<Option<Part>>) -> Node {
    if of.iter().all(Option::is_some) {
        return Node { told: told.clone(), of: of.into_iter().flatten().collect() };
    }
    fn kept<T: Clone>(
        items: &[T],
        parts: &mut impl Iterator<Item = Option<Part>>,
        kept: &mut Vec<Part>,
    ) -> Box<[T]> {
        let told = |_: &&T| match parts.next().flatten() {
            Some(part) => {
                kept.push(part);
                true
            }
            None => false,
        };
        items.iter().filter(told).cloned().collect()
    }
    type Parts = std::vec::IntoIter<Option<Part>>;
    let shape = |it: &Shape, parts: &mut Parts, of: &mut Vec<Part>| Shape {
        name: it.name.clone(),
        size: it.size,
        fields: kept(&it.fields, parts, of),
        parameters: kept(&it.parameters, parts, of),
    };
    let (mut parts, mut of) = (of.into_iter(), Vec::new());
    let told = match &**told {
        Told::Pointer { name } => {
            of.extend(parts.flatten());
            Told::Pointer { name: name.clone() }
        }
        Told::Struct(it) => Told::Struct(shape(it, &mut parts, &mut of)),
        Told::Union(it) => Told::Union(shape(it, &mut parts, &mut of)),
        Told::Enum { of: it, tag, variants } => {
            let it = shape(it, &mut parts, &mut of);
            let variants = variants
                .iter()
                .map(|variant| Shaped {
                    name: variant.name.clone(),
                    tag: variant.tag,
                    fields: kept(&variant.fields, &mut parts, &mut of),
                })
                .collect();
            Told::Enum { of: it, tag: *tag, variants }
        }
        // An array of a type not told of is not told of ([`Search::own`]), and these are
        // made of no type.
        Told::Array { .. } | Told::Base { .. } | Told::Names { .. } => {
            return Node { told: told.clone(), of: Box::new([]) };
        }
    };
    Node { told: Interned::new(told), of: of.into() }
}

/// What a debugger is told of a type apart from where in its set the types it is made of are,
/// which the type is made of in the order [`TypeSet::get`] tells.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) enum Told {
    Base {
        name: Symbol,
        size: u64,
        encoding: u8,
    },
    /// Made of what it points to where that is told.
    Pointer {
        name: Symbol,
    },
    /// Made of its element.
    Array {
        len: u64,
    },
    /// Made of the types of its fields, then of what its generic parameters stand for.
    Struct(Shape),
    Union(Shape),
    Names {
        name: Symbol,
        size: u64,
        names: Box<[(Symbol, u64)]>,
    },
    /// Made of what [`Told::Struct`] is, then of the types of the fields of each variant.
    Enum {
        of: Shape,
        tag: Option<Tag>,
        variants: Box<[Shaped]>,
    },
}

impl_internable!(Told);

impl Told {
    /// The name a debugger knows the type by, where it has one.
    fn name(&self) -> Option<&Symbol> {
        match self {
            Told::Base { name, .. } | Told::Pointer { name } | Told::Names { name, .. } => {
                Some(name)
            }
            Told::Struct(it) | Told::Union(it) | Told::Enum { of: it, .. } => Some(&it.name),
            Told::Array { .. } => None,
        }
    }
}

/// An [`Aggregate`] without the places of its types.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Shape {
    pub(super) name: Symbol,
    pub(super) size: u64,
    /// The name and the offset of each field.
    pub(super) fields: Box<[(Symbol, u64)]>,
    pub(super) parameters: Box<[Symbol]>,
}

/// A [`Variant`] without the places of the types of its fields.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Shaped {
    pub(super) name: Symbol,
    pub(super) tag: Option<u64>,
    pub(super) fields: Box<[(Symbol, u64)]>,
}

impl TypeSet {
    /// What tells the set from every other one there is while it is.
    pub(crate) fn at(&self) -> usize {
        std::ptr::from_ref(self.node()).addr()
    }

    pub(super) fn node(&self) -> &Node {
        &self.group[self.nth as usize]
    }

    /// The types it is made of, in the order [`Told`] says.
    pub(super) fn of(&self) -> impl Iterator<Item = Of<'_>> {
        self.node().of.iter().map(|part| match part {
            Part::Type(set) => Of::Type(set.clone()),
            Part::Here(nth) => Of::Type(TypeSet { group: self.group.clone(), nth: *nth }),
            Part::Named(name) => Of::Named(name),
        })
    }

    /// The set as a list of types, the type itself first, which is how the store keeps it.
    pub(crate) fn to_vec(&self) -> Vec<DebugType> {
        let mut flat = Flat::default();
        flat.add(self);
        flat.canonical()
    }

    /// The set of the first of `types`, which name each other by their places among them.
    fn of_list(types: &[DebugType]) -> TypeSet {
        let mut listed = Listed { types, sets: FxHashMap::default() };
        resolve(&mut listed, &0).unwrap_or_else(|| {
            let told = Told::Struct(Shape {
                name: Symbol::intern("()"),
                size: 0,
                fields: Box::new([]),
                parameters: Box::new([]),
            });
            let node = Node { told: Interned::new(told), of: Box::new([]) };
            TypeSet { group: Arc::new([node]), nth: 0 }
        })
    }
}

impl PartialEq for TypeSet {
    /// Whether the two tell the same: types made of each other are the same where nothing
    /// else tells them apart.
    fn eq(&self, other: &Self) -> bool {
        fn same(one: &TypeSet, other: &TypeSet, assumed: &mut FxHashSet<(usize, usize)>) -> bool {
            if one.at() == other.at() || !assumed.insert((one.at(), other.at())) {
                return true;
            }
            one.node().told == other.node().told
                && one.node().of.len() == other.node().of.len()
                && one.of().zip(other.of()).all(|pair| match pair {
                    (Of::Type(one), Of::Type(other)) => same(&one, &other, assumed),
                    (Of::Named(one), Of::Named(other)) => one == other,
                    _ => false,
                })
        }
        same(self, other, &mut FxHashSet::default())
    }
}

impl Eq for TypeSet {}

impl From<&[DebugType]> for TypeSet {
    fn from(types: &[DebugType]) -> Self {
        TypeSet::of_list(types)
    }
}

impl From<Vec<DebugType>> for TypeSet {
    fn from(types: Vec<DebugType>) -> Self {
        TypeSet::of_list(&types)
    }
}

impl fmt::Debug for TypeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.to_vec()).finish()
    }
}

/// A set being written as a list ([`TypeSet::to_vec`]).
#[derive(Default)]
struct Flat {
    /// Each type of the list, with the places of the types it is made of: `None` for a name
    /// not looked up yet.
    types: Vec<(Interned<Told>, Vec<Option<TypeIndex>>)>,
    /// The place of each type already in the list, by where it is.
    places: FxHashMap<usize, TypeIndex>,
    /// The first type of each name in the list.
    names: FxHashMap<Symbol, TypeIndex>,
    /// The [`Part::Named`]s, by the type and the part, to look up once all are in.
    named: Vec<(TypeIndex, usize, Symbol)>,
}

impl Flat {
    fn add(&mut self, set: &TypeSet) -> TypeIndex {
        let root = set.at();
        let mut pending: Vec<(TypeSet, Option<(usize, usize)>)> = vec![(set.clone(), None)];
        while let Some((set, parent)) = pending.pop() {
            let known = self.places.get(&set.at()).copied();
            let at = known.unwrap_or(self.types.len() as TypeIndex);
            if let Some((parent, nth)) = parent {
                self.types[parent].1[nth] = Some(at);
            }
            if known.is_some() {
                continue;
            }
            let told = set.node().told.clone();
            if let Some(name) = told.name() {
                self.names.entry(name.clone()).or_insert(at);
            }
            let parts: Vec<_> = set.of().collect();
            self.types.push((told, vec![None; parts.len()]));
            self.places.insert(set.at(), at);
            for (nth, part) in parts.into_iter().enumerate().rev() {
                match part {
                    Of::Type(set) => pending.push((set, Some((at as usize, nth)))),
                    Of::Named(name) => self.named.push((at, nth, name.clone())),
                }
            }
        }
        self.places[&root]
    }

    fn canonical(mut self) -> Vec<DebugType> {
        let mut unresolved = vec![Vec::new(); self.types.len()];
        for (at, nth, name) in self.named {
            let target = self.names.get(&name).copied();
            self.types[at as usize].1[nth] = target;
            if target.is_none() {
                unresolved[at as usize].push((nth, name));
            }
        }
        for names in &mut unresolved {
            names.sort_unstable_by_key(|(nth, _)| *nth);
        }

        // Resolving the same recursive types concurrently can leave independent but
        // equivalent groups in a set. Partition by all metadata and ordered edges,
        // rather than by allocation sharing, before assigning the stored type indices.
        let mut groups = FxHashMap::default();
        let mut blocks: Vec<Vec<usize>> = Vec::new();
        let mut classes = Vec::with_capacity(self.types.len());
        let mut positions = Vec::with_capacity(self.types.len());
        let mut incoming = vec![Vec::new(); self.types.len()];
        for (at, (told, of)) in self.types.iter().enumerate() {
            let class = *groups.entry((told, of.len(), &unresolved[at])).or_insert_with(|| {
                blocks.push(Vec::new());
                blocks.len() - 1
            });
            classes.push(class);
            positions.push(blocks[class].len());
            blocks[class].push(at);
            for (nth, target) in of.iter().enumerate() {
                if let Some(target) = target {
                    incoming[*target as usize].push((at, nth));
                }
            }
        }

        let mut pending: std::collections::VecDeque<_> = (0..blocks.len()).collect();
        let mut queued = vec![true; blocks.len()];
        while let Some(target) = pending.pop_front() {
            queued[target] = false;
            let mut edges = FxHashMap::<_, Vec<_>>::default();
            for &at in &blocks[target] {
                for &(parent, nth) in &incoming[at] {
                    edges.entry(nth).or_default().push(parent);
                }
            }
            for parents in edges.into_values() {
                let mut touched = FxHashMap::<_, Vec<_>>::default();
                for parent in parents {
                    touched.entry(classes[parent]).or_default().push(parent);
                }
                for (class, split) in touched {
                    if split.len() == blocks[class].len() {
                        continue;
                    }
                    let new = blocks.len();
                    blocks.push(Vec::with_capacity(split.len()));
                    queued.push(false);
                    for at in split {
                        let position = positions[at];
                        blocks[class].swap_remove(position);
                        if let Some(&swapped) = blocks[class].get(position) {
                            positions[swapped] = position;
                        }
                        classes[at] = new;
                        positions[at] = blocks[new].len();
                        blocks[new].push(at);
                    }
                    // Each edge is revisited only for smaller splitters, unless both
                    // halves replace a block that was already waiting in the queue.
                    let next = if queued[class] || blocks[new].len() < blocks[class].len() {
                        new
                    } else {
                        class
                    };
                    if !queued[next] {
                        queued[next] = true;
                        pending.push_back(next);
                    }
                }
            }
        }

        let mut order = Vec::new();
        let mut indices = vec![None; blocks.len()];
        let mut pending = vec![0];
        while let Some(at) = pending.pop() {
            let class = classes[at];
            if indices[class].is_some() {
                continue;
            }
            indices[class] = Some(order.len() as TypeIndex);
            order.push(at);
            pending.extend(self.types[at].1.iter().rev().flatten().map(|&at| at as usize));
        }
        order
            .into_iter()
            .map(|at| {
                let (told, of) = &self.types[at];
                let of: Vec<_> =
                    of.iter().map(|at| at.and_then(|at| indices[classes[at as usize]])).collect();
                expand(told, &of)
            })
            .collect()
    }
}

/// A list of types read from the store, for [`resolve`] to put a set together from.
struct Listed<'a> {
    types: &'a [DebugType],
    sets: FxHashMap<TypeIndex, Option<TypeSet>>,
}

impl Found for Listed<'_> {
    type Key = TypeIndex;

    fn own(&mut self, key: &TypeIndex) -> Option<Arc<Own<TypeIndex>>> {
        let mut of = Vec::new();
        let told = Interned::new(shape(self.types.get(*key as usize)?, &mut of));
        Some(Arc::new(Own { told, of: of.into_iter().map(Made::Key).collect() }))
    }

    fn set(&mut self, key: &TypeIndex) -> Option<Option<TypeSet>> {
        self.sets.get(key).cloned()
    }

    fn keep(&mut self, key: &TypeIndex, set: Option<TypeSet>) {
        self.sets.insert(*key, set);
    }
}

/// What `ty` is told as, with the places of the types it is made of pushed to `of`.
pub(super) fn shape(ty: &DebugType, of: &mut Vec<TypeIndex>) -> Told {
    let names = |fields: &[Field], of: &mut Vec<TypeIndex>| -> Box<[(Symbol, u64)]> {
        of.extend(fields.iter().map(|it| it.ty));
        fields.iter().map(|it| (Symbol::intern(&it.name), it.offset)).collect()
    };
    let aggregate = |it: &Aggregate, of: &mut Vec<TypeIndex>| {
        let fields = names(&it.fields, of);
        of.extend(it.parameters.iter().map(|(_, ty)| *ty));
        let parameters = it.parameters.iter().map(|(name, _)| Symbol::intern(name)).collect();
        Shape { name: Symbol::intern(&it.name), size: it.size, fields, parameters }
    };
    match ty {
        DebugType::Base { name, size, encoding } => {
            Told::Base { name: Symbol::intern(name), size: *size, encoding: *encoding }
        }
        DebugType::Pointer { name, to } => {
            of.extend(*to);
            Told::Pointer { name: Symbol::intern(name) }
        }
        DebugType::Array { of: element, len } => {
            of.push(*element);
            Told::Array { len: *len }
        }
        DebugType::Struct(it) => Told::Struct(aggregate(it, of)),
        DebugType::Union(it) => Told::Union(aggregate(it, of)),
        DebugType::Names { name, size, names } => Told::Names {
            name: Symbol::intern(name),
            size: *size,
            names: names.iter().map(|(name, value)| (Symbol::intern(name), *value)).collect(),
        },
        DebugType::Enum { of: it, tag, variants } => {
            let it = aggregate(it, of);
            let variants = variants
                .iter()
                .map(|it| Shaped {
                    name: Symbol::intern(&it.name),
                    tag: it.tag,
                    fields: names(&it.fields, of),
                })
                .collect();
            Told::Enum { of: it, tag: *tag, variants }
        }
    }
}

/// The type `told` is, made of the types at `of`: one at `None` is left out.
fn expand(told: &Told, of: &[Option<TypeIndex>]) -> DebugType {
    type Places<'a> = std::iter::Copied<std::slice::Iter<'a, Option<TypeIndex>>>;
    fn fields(names: &[(Symbol, u64)], of: &mut Places<'_>) -> Vec<Field> {
        let fields = names.iter().zip(of);
        let told = fields.filter_map(|(it, ty)| Some((it, ty?)));
        told.map(|((name, offset), ty)| Field { name: name.to_string(), offset: *offset, ty })
            .collect()
    }
    fn aggregate(it: &Shape, of: &mut Places<'_>) -> Aggregate {
        let fields = fields(&it.fields, of);
        let parameters = it.parameters.iter().zip(of).filter_map(|(name, ty)| Some((name, ty?)));
        Aggregate {
            name: it.name.to_string(),
            size: it.size,
            fields,
            parameters: parameters.map(|(name, ty)| (name.to_string(), ty)).collect(),
        }
    }
    let mut of = of.iter().copied();
    match told {
        Told::Base { name, size, encoding } => {
            DebugType::Base { name: name.to_string(), size: *size, encoding: *encoding }
        }
        Told::Pointer { name } => {
            DebugType::Pointer { name: name.to_string(), to: of.next().flatten() }
        }
        Told::Array { len } => {
            DebugType::Array { of: of.next().flatten().unwrap_or_default(), len: *len }
        }
        Told::Struct(it) => DebugType::Struct(aggregate(it, &mut of)),
        Told::Union(it) => DebugType::Union(aggregate(it, &mut of)),
        Told::Names { name, size, names } => DebugType::Names {
            name: name.to_string(),
            size: *size,
            names: names.iter().map(|(name, value)| (name.to_string(), *value)).collect(),
        },
        Told::Enum { of: it, tag, variants } => {
            let it = aggregate(it, &mut of);
            let variants = variants.iter().map(|variant| {
                let fields = fields(&variant.fields, &mut of);
                Variant { name: variant.name.to_string(), tag: variant.tag, fields }
            });
            DebugType::Enum { of: it, tag: *tag, variants: variants.collect() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, offset: u64, ty: TypeIndex) -> Field {
        Field { name: name.into(), offset, ty }
    }

    fn aggregate(
        name: &str,
        size: u64,
        fields: Vec<Field>,
        parameters: Vec<(String, TypeIndex)>,
    ) -> Aggregate {
        Aggregate { name: name.into(), size, fields, parameters }
    }

    /// `Option<Box<List>>` where a `List` holds a `u8`, the `Option` of the next one and more:
    /// a pointer back into the set, an enum with a tag, a generic parameter and an array, in
    /// the order a set is written in.
    fn list() -> Vec<DebugType> {
        vec![
            DebugType::Enum {
                of: aggregate("Option<Box<List>>", 8, vec![], vec![("T".into(), 1)]),
                tag: Some(Tag { offset: 0, size: 8, states: false }),
                variants: vec![
                    Variant { name: "None".into(), tag: Some(0), fields: vec![] },
                    Variant { name: "Some".into(), tag: None, fields: vec![field("__0", 0, 1)] },
                ],
            },
            DebugType::Pointer { name: "Box<List>".into(), to: Some(2) },
            DebugType::Struct(aggregate(
                "List",
                40,
                vec![
                    field("value", 8, 3),
                    field("next", 0, 0),
                    field("pad", 9, 4),
                    field("raw", 16, 5),
                    field("order", 24, 6),
                    field("slot", 25, 7),
                ],
                vec![],
            )),
            DebugType::Base { name: "u8".into(), size: 1, encoding: 8 },
            DebugType::Array { of: 3, len: 7 },
            DebugType::Pointer { name: "*const ()".into(), to: None },
            DebugType::Names {
                name: "Ordering".into(),
                size: 1,
                names: vec![("Less".into(), 255), ("Equal".into(), 0)],
            },
            DebugType::Union(aggregate(
                "MaybeUninit<u8>",
                1,
                vec![field("value", 0, 3)],
                vec![("T".into(), 3)],
            )),
        ]
    }

    #[test]
    fn a_set_is_what_it_was_made_of() {
        let set = TypeSet::from(list());
        assert_eq!(set.to_vec(), list());
        assert!(TypeSet::from(set.to_vec()) == set);
    }

    /// The type at `places` among the ones `set` is made of, and the ones they are.
    fn part(set: &TypeSet, places: &[usize]) -> TypeSet {
        let mut part = set.clone();
        for &at in places {
            let Some(Of::Type(set)) = part.of().nth(at) else { panic!("no type at {places:?}") };
            part = set;
        }
        part
    }

    #[test]
    fn a_type_in_a_set_twice_is_one() {
        let set = TypeSet::from(list());
        // The `u8` of `List::value`, and of the field and the parameter of `MaybeUninit<u8>`.
        let value = part(&set, &[0, 0, 0]).at();
        assert!(value == part(&set, &[0, 0, 5, 0]).at() && value == part(&set, &[0, 0, 5, 1]).at());
        // `List::next` is the set itself.
        assert_eq!(part(&set, &[0, 0, 1]).at(), set.at());
    }

    /// Types by their names, each made of the ones it names.
    #[derive(Default)]
    struct Named(Vec<(&'static str, Vec<&'static str>)>, FxHashMap<&'static str, Option<TypeSet>>);

    impl Found for Named {
        type Key = &'static str;

        fn own(&mut self, key: &&'static str) -> Option<Arc<Own<&'static str>>> {
            let (_, of) = self.0.iter().find(|(name, _)| name == key)?;
            let parameters = of.iter().map(|it| Symbol::intern(it)).collect();
            let shape =
                Shape { name: Symbol::intern(key), size: 8, fields: Box::new([]), parameters };
            let told = Interned::new(Told::Struct(shape));
            Some(Arc::new(Own { told, of: of.iter().map(|it| Made::Key(*it)).collect() }))
        }

        fn set(&mut self, key: &&'static str) -> Option<Option<TypeSet>> {
            self.1.get(key).cloned()
        }

        fn keep(&mut self, key: &&'static str, set: Option<TypeSet>) {
            self.1.insert(key, set);
        }
    }

    #[test]
    fn a_type_made_of_itself_through_a_parameter() {
        // `struct Tree { children: Vec<Tree> }`, as the parameter of the `Vec` names it.
        let types = vec![("Tree", vec!["Vec<Tree>", "u8"]), ("Vec<Tree>", vec!["Tree"])];
        let mut found = Named(types, FxHashMap::default());
        let tree = resolve(&mut found, &"Tree").unwrap();
        assert_eq!(part(&tree, &[0, 0]).at(), tree.at());
        // The `u8`, which nothing tells of, is left out with its parameter.
        assert_eq!(tree.of().count(), 1);
        // The `Vec<Tree>` of every other set is this one's.
        let vec = resolve(&mut found, &"Vec<Tree>").unwrap();
        assert_eq!(vec.at(), part(&tree, &[0]).at());
        let names: Vec<String> = tree
            .to_vec()
            .iter()
            .map(|it| match it {
                DebugType::Struct(it) => format!("{} {:?}", it.name, it.parameters),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["Tree [(\"Vec<Tree>\", 1)]", "Vec<Tree> [(\"Tree\", 0)]"]);
    }

    #[test]
    fn types_made_of_each_other_in_many_ways_are_put_together_once() {
        // Each of 40 types is made of every other one, as the types of a syntax tree are of
        // each other through `Vec`s and `Option`s: a set put together for each way to
        // reach a type would never be done.
        let names: Vec<&'static str> =
            (0..40).map(|at| &*Box::leak(format!("T{at}").into_boxed_str())).collect();
        let types = names.iter().map(|name| (*name, names.clone())).collect();
        let mut found = Named(types, FxHashMap::default());
        let set = resolve(&mut found, &names[0]).unwrap();
        assert_eq!(set.to_vec().len(), 40);
        assert_eq!(found.1.len(), 40);
    }

    fn pair(left: TypeSet, right: TypeSet) -> TypeSet {
        TypeSet {
            group: Arc::new([Node {
                told: Interned::new(Told::Struct(Shape {
                    name: Symbol::intern("Pair"),
                    size: 16,
                    fields: Box::new([(Symbol::intern("left"), 0), (Symbol::intern("right"), 8)]),
                    parameters: Box::new([]),
                })),
                of: Box::new([Part::Type(left), Part::Type(right)]),
            }]),
            nth: 0,
        }
    }

    fn recursive() -> TypeSet {
        TypeSet::from(vec![
            DebugType::Struct(aggregate("A", 8, vec![field("b", 0, 1)], vec![])),
            DebugType::Struct(aggregate("B", 8, vec![field("a", 0, 0)], vec![])),
        ])
    }

    #[test]
    fn equal_graphs_have_identical_bytes_regardless_of_sharing() {
        let one = TypeSet::from(list());
        let shared = pair(one.clone(), one);
        let duplicated = pair(TypeSet::from(list()), TypeSet::from(list()));
        assert!(shared == duplicated);
        assert_eq!(shared.to_vec(), duplicated.to_vec());
        assert_eq!(
            postcard::to_allocvec(&shared.to_vec()),
            postcard::to_allocvec(&duplicated.to_vec())
        );
    }

    #[test]
    fn mutually_recursive_graphs_have_identical_bytes_regardless_of_sharing() {
        let one = recursive();
        let shared = pair(one.clone(), one);
        let duplicated = pair(recursive(), recursive());
        assert!(shared == duplicated);
        assert_eq!(shared.to_vec(), duplicated.to_vec());
        assert_eq!(shared.to_vec().len(), 3);
        assert!(TypeSet::from(duplicated.to_vec()) == shared);
    }

    #[test]
    fn local_named_edges_and_direct_edges_have_identical_bytes() {
        let direct = TypeSet::from(vec![DebugType::Struct(aggregate(
            "Recursive",
            8,
            vec![field("next", 0, 0)],
            vec![],
        ))]);
        let named = TypeSet {
            group: Arc::new([Node {
                told: direct.node().told.clone(),
                of: Box::new([Part::Named(Symbol::intern("Recursive"))]),
            }]),
            nth: 0,
        };
        assert_eq!(pair(direct.clone(), direct.clone()).to_vec(), pair(direct, named).to_vec());
    }

    #[test]
    fn equal_names_do_not_merge_distinct_type_metadata() {
        let original = aggregate("Same", 16, vec![field("value", 0, 1)], vec![("T".into(), 1)]);
        let base = DebugType::Base { name: "u8".into(), size: 1, encoding: 7 };
        let graph = |of| TypeSet::from(vec![DebugType::Struct(of), base.clone()]);
        let mut variants = Vec::new();
        let mut changed = original.clone();
        changed.size = 32;
        variants.push(changed);
        let mut changed = original.clone();
        changed.fields[0].name = "other".into();
        variants.push(changed);
        let mut changed = original.clone();
        changed.fields[0].offset = 8;
        variants.push(changed);
        let mut changed = original.clone();
        changed.parameters[0].0 = "U".into();
        variants.push(changed);
        for variant in variants {
            let left = graph(original.clone());
            let right = graph(variant);
            assert!(left != right);
            let combined = pair(left, right);
            let encoded = combined.to_vec();
            let same = encoded
                .iter()
                .filter(|ty| matches!(ty, DebugType::Struct(it) if it.name == "Same"))
                .count();
            assert_eq!(same, 2);
            assert!(TypeSet::from(encoded) == combined);
        }
    }

    #[test]
    fn equal_layouts_retain_distinct_targets_and_field_order() {
        let graph = |encoding| {
            TypeSet::from(vec![
                DebugType::Struct(aggregate("Same", 1, vec![field("value", 0, 1)], vec![])),
                DebugType::Base { name: "byte".into(), size: 1, encoding },
            ])
        };
        let unsigned = graph(7);
        let signed = graph(5);
        let forward = pair(unsigned.clone(), signed.clone()).to_vec();
        let reverse = pair(signed, unsigned).to_vec();
        assert_ne!(forward, reverse);
        assert_eq!(forward.len(), 5);
        assert_eq!(
            forward
                .iter()
                .filter(|ty| matches!(ty, DebugType::Struct(it) if it.name == "Same"))
                .count(),
            2
        );
    }

    #[test]
    fn unresolved_named_edges_remain_distinct() {
        let pointer = |name| TypeSet {
            group: Arc::new([Node {
                told: Interned::new(Told::Pointer { name: Symbol::intern("opaque") }),
                of: Box::new([Part::Named(Symbol::intern(name))]),
            }]),
            nth: 0,
        };
        let encoded = pair(pointer("ExternalA"), pointer("ExternalB")).to_vec();
        assert_eq!(encoded.len(), 3);
        assert_eq!(encoded[1], DebugType::Pointer { name: "opaque".into(), to: None });
        assert_eq!(encoded[2], encoded[1]);
    }

    #[test]
    fn long_chains_are_partitioned_and_written_without_recursive_traversal() {
        let mut sets =
            vec![TypeSet::from(vec![DebugType::Struct(aggregate("Link", 8, vec![], vec![]))])];
        let told = Interned::new(Told::Struct(Shape {
            name: Symbol::intern("Link"),
            size: 8,
            fields: Box::new([(Symbol::intern("next"), 0)]),
            parameters: Box::new([]),
        }));
        for _ in 0..8192 {
            let child = sets.last().unwrap().clone();
            sets.push(TypeSet {
                group: Arc::new([Node { told: told.clone(), of: Box::new([Part::Type(child)]) }]),
                nth: 0,
            });
        }
        let encoded = sets.last().unwrap().to_vec();
        assert_eq!(encoded.len(), sets.len());
        for (nth, ty) in encoded.iter().enumerate() {
            let DebugType::Struct(it) = ty else { panic!("not a link") };
            if nth + 1 == encoded.len() {
                assert!(it.fields.is_empty());
            } else {
                assert_eq!(it.fields, [field("next", 0, (nth + 1) as TypeIndex)]);
            }
        }
        // Drop each owner before its child rather than testing Arc's recursive destructor.
        while sets.pop().is_some() {}
    }

    #[test]
    fn overlapping_resolutions_in_one_cache_have_identical_bytes() {
        struct Shared {
            definitions: Named,
            cache: Arc<std::sync::Mutex<FxHashMap<&'static str, Option<TypeSet>>>>,
            start: Arc<std::sync::Barrier>,
        }
        impl Found for Shared {
            type Key = &'static str;

            fn own(&mut self, key: &&'static str) -> Option<Arc<Own<Self::Key>>> {
                if *key == "A" {
                    self.start.wait();
                }
                self.definitions.own(key)
            }

            fn set(&mut self, key: &&'static str) -> Option<Option<TypeSet>> {
                self.cache.lock().unwrap().get(key).cloned()
            }

            fn keep(&mut self, key: &&'static str, set: Option<TypeSet>) {
                self.cache.lock().unwrap().insert(key, set);
            }
        }
        let cache = Arc::new(std::sync::Mutex::new(FxHashMap::default()));
        let start = Arc::new(std::sync::Barrier::new(4));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|nth| {
                    let mut found = Shared {
                        definitions: Named(
                            vec![("A", vec!["B"]), ("B", vec!["A"])],
                            FxHashMap::default(),
                        ),
                        cache: cache.clone(),
                        start: start.clone(),
                    };
                    scope.spawn(move || {
                        let first = if nth % 2 == 0 { "A" } else { "B" };
                        resolve(&mut found, &first).unwrap();
                        resolve(&mut found, &"A").unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|it| it.join().unwrap()).collect::<Vec<_>>()
        });
        let expected = results[0].to_vec();
        for set in &results {
            assert!(set == &results[0]);
            assert_eq!(set.to_vec(), expected);
        }
        assert_eq!(
            pair(results[0].clone(), results[0].clone()).to_vec(),
            pair(results[1].clone(), results[2].clone()).to_vec()
        );
    }

    #[test]
    fn concurrently_resolved_graphs_have_stable_store_keys_and_entry_references() {
        use crate::{cached, debuginfo::FunctionDebug, fragment::Code, store};
        use cranelift_codegen::{ir::Signature, isa::CallConv};

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let resolved = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|nth| {
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        let mut found =
                            Named(vec![("A", vec!["B"]), ("B", vec!["A"])], FxHashMap::default());
                        barrier.wait();
                        let first = if nth % 2 == 0 { "A" } else { "B" };
                        resolve(&mut found, &first).unwrap();
                        resolve(&mut found, &"A").unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|it| it.join().unwrap()).collect::<Vec<_>>()
        });
        let shared = pair(resolved[0].clone(), resolved[0].clone());
        let interleaved = pair(resolved[1].clone(), resolved[2].clone());
        assert!(shared == interleaved);

        store::compiles_against("1.99.0");
        let directory =
            std::env::temp_dir().join(format!("blaze-debug-graph-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let store = store::Store::owned(&directory).unwrap();
        let encode = |types| {
            let mut debug = FunctionDebug::new("test".into(), "test".into());
            debug.types = vec![types];
            let code = Code {
                signature: Signature::new(CallConv::SystemV),
                bytes: vec![],
                align: 1,
                relocs: vec![],
                elf_types: vec![],
                unwind: None,
                lines: vec![],
                slots: Default::default(),
            };
            let mut entry = cached::Entry {
                label: "test".into(),
                semantic: None,
                funcs: vec![],
                datas: vec![],
                code: (&code).into(),
                debug: Some(debug),
                tracked: None,
            };
            let bytes = cached::Writer::alone(&store, None, None).encoded(&mut entry).unwrap();
            let references = &entry.debug.as_ref().unwrap().stored_types;
            assert_eq!(references.len(), 1);
            store.seal_checked().unwrap();
            assert!(store.get(&references[0]).is_some());
            assert_eq!(
                cached::Entry::decode(&bytes).unwrap().debug.unwrap().stored_types,
                *references
            );
            (bytes, references.clone())
        };
        let first = encode(shared);
        let second = encode(interleaved);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
        assert_eq!(first.1, second.1);
        assert_eq!(first.0, second.0);
    }
}
