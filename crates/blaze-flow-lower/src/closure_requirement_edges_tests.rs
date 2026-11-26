use blaze_flow::Origin;

use super::{External, Need, Requirement, requirement};
use crate::region_fixture::alloc;

const CAPTURE: Origin = Origin(0);
const SCOPE_REF: Origin = Origin(1);
const SCOPE_ARG: Origin = Origin(2);
const ENV: Origin = Origin(3);
const OUTER: Origin = Origin(4);
const STATIC: Origin = Origin(5);

const EXTERNALS: &[(Origin, External)] = &[
    (CAPTURE, External::Capture(0)),
    (SCOPE_REF, External::Late),
    (SCOPE_ARG, External::Late),
    (ENV, External::SigOwner(0)),
    (OUTER, External::SigOwner(1)),
    (STATIC, External::Static),
];

#[test]
fn a_scoped_callback_requires_its_capture_to_outlive_the_environment() {
    // `&'scope Scope<'scope, 'env>` names `'scope` twice and implies `'env: 'scope`.
    // The owner's `'outer: 'env` also suffices, but proving it for the capture is stronger.
    let known = [(SCOPE_REF, SCOPE_ARG), (SCOPE_ARG, SCOPE_REF), (ENV, SCOPE_REF), (OUTER, ENV)];
    let got = requirement(EXTERNALS, &known, CAPTURE, SCOPE_ARG, alloc());
    assert_eq!(got, Some(Requirement { from: Need::Capture(0), into: Need::Sig(0) }));
}

#[test]
fn a_scoped_callback_needs_an_unambiguous_owner_bound_in_the_right_direction() {
    // These controls remove, reverse, or make the owner bound ambiguous. The old requirement
    // still cannot be stated in the owner, including when only `'static` outlives the scope.
    for known in [
        &[][..],
        &[(SCOPE_REF, ENV)][..],
        &[(STATIC, SCOPE_REF)][..],
        &[(ENV, SCOPE_REF), (OUTER, SCOPE_REF)][..],
    ] {
        assert_eq!(requirement(EXTERNALS, known, CAPTURE, SCOPE_REF, alloc()), None, "known: {known:?}");
    }
    assert_eq!(requirement(EXTERNALS, &[(ENV, SCOPE_REF)], SCOPE_REF, CAPTURE, alloc()), None);
}
