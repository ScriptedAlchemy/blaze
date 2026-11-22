//! A scalar constant's query asked after an edit that remakes the session arena: what failed
//! while hir-ty homed plain data (a scalar, a type of no item) at `ArenaId::SESSION`, the
//! session's generation 0, and the first commit after it dropped that generation ("arena 0.0
//! is dead").

use base_db::{Db, Env};
use blaze_memo::Database as _;
use hir_def::{ConstId, ModuleDefId, nameres::crate_def_map};
use hir_expand::fixture;

use crate::{
    db::{GeneralConstId, HirDatabase},
    next_solver::GenericArgs,
};

const FIXTURE: &str = r#"
//- minicore: sized
//- /main.rs crate:main
const GOAL: u32 = 7;
"#;

/// The bytes of the fixture crate's constant `GOAL`, evaluated in `db`'s snapshot.
fn goal(db: &Db) -> Vec<u8> {
    blaze_memo::attach(db, || {
        let krate = fixture::test_crate(db);
        let goal = crate_def_map(db, krate)
            .modules(db)
            .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::ConstId(it) if GeneralConstId::ConstId(it).name(db) == "GOAL" => {
                    Some::<ConstId>(it)
                }
                _ => None,
            })
            .expect("the fixture's `GOAL`");
        let value = db
            .const_eval(goal, GenericArgs::empty(), None)
            .unwrap_or_else(|_| panic!("`GOAL` did not evaluate"));
        value.memory.to_vec()
    })
}

/// The invariant: a scalar constant's query answers in a snapshot whose session arena was
/// dropped and made again by a commit (an edit of the constant's file, and of the process's
/// variables, a session-level input), with the edited value. The control is the generation:
/// the commit did remake the session, so a home at its first generation would be dead.
#[test]
fn a_scalar_constant_answers_after_an_edit_remakes_the_session() {
    let mut fixture = fixture::db(FIXTURE);
    assert_eq!(goal(fixture.db()), 7_u32.to_le_bytes());
    let before = fixture.db().memo().session();
    let file = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut vars = Env::default();
    vars.set("BLAZE_SESSION_EDIT", "1");
    _ = fixture.edit(|edit| _ = edit.set_file_text(file, "const GOAL: u32 = 9;\n").set_process_env(vars));
    let after = fixture.db().memo().session();
    assert_eq!(after.index(), before.index());
    assert!(
        after.generation() > before.generation(),
        "the commit remade the session arena ({before:?} then {after:?})"
    );
    assert_eq!(goal(fixture.db()), 9_u32.to_le_bytes(), "the edited value, asked at the new session");
}

#[test]
fn the_oracle_catches_a_value_homed_in_the_old_session_generation() {
    let mut fixture = fixture::db(FIXTURE);
    let before = fixture.db().memo().session();
    assert!(fixture.db().memo().live(before).is_ok());
    assert_eq!(goal(fixture.db()), 7_u32.to_le_bytes());
    let mut vars = Env::default();
    vars.set("BLAZE_SESSION_EDIT", "1");
    _ = fixture.edit(|edit| _ = edit.set_process_env(vars));
    let memo = fixture.db().memo();
    let after = memo.session();
    assert!(memo.live(after).is_ok());
    // Plant the stale home, with the same arena index but the writer's old generation.
    let dead = memo.live(before).err().expect("the old session's home is refused");
    assert_eq!(dead.named, before);
    assert_eq!(dead.held, Some(after));
    assert_eq!(goal(fixture.db()), 7_u32.to_le_bytes());
}
