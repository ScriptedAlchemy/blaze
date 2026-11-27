//! Command-granted environment values as individual engine facts.
use blaze_build_script::env::CallerEnv;
use blaze_drive::{Kind, Name};
use blaze_engine::{CellId, Planned};
use blaze_ids::ContentId;

/// A reader observes absence at this same address before a grant and after its removal.
pub(crate) fn cell(variable: &str) -> CellId {
    CellId {
        of: Name::new(Kind::Given, ContentId::of("blaze-cli caller env name 1", variable.as_bytes())),
        cell: crate::engine_tasks::WHOLE,
    }
}

/// The answer is the command edge's private digest. An absent variable needs no producer:
/// retiring its old plan publishes the engine's absent cell and wakes precisely its readers.
pub(crate) fn plan<'a>(caller: CallerEnv<'a>) -> impl Iterator<Item = (Planned, ContentId)> + 'a {
    caller.rows().iter().filter_map(|&(variable, value, answer)| {
        value?;
        let name = cell(variable).of;
        Some((Planned { name, file: name, source: answer, phase: crate::engine_script_reads::PHASE }, answer))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_present_variables_publish_at_the_readers_address() {
        let empty = ContentId([7; 32]);
        let first = ContentId([8; 32]);
        let second = ContentId([9; 32]);
        let absent = blaze_drive::key::ABSENT;
        let before_rows = [("APP_A", Some(""), empty), ("APP_B", None, absent)];
        let after_rows = [("APP_A", Some(""), empty), ("APP_B", Some("set"), first)];
        let before = CallerEnv::new(&before_rows, first).unwrap();
        let after = CallerEnv::new(&after_rows, second).unwrap();
        let same = |env| plan(env).find(|(one, _)| one.name == cell("APP_A").of).unwrap();
        assert_eq!(same(before).0.source, same(after).0.source);
        assert_eq!(same(before).1, empty);
        assert_eq!(cell("APP_A").cell, crate::engine_tasks::WHOLE);
        assert_eq!(before.answer("APP_B"), absent);
        assert!(plan(before).all(|(one, _)| one.name != cell("APP_B").of));
        assert!(plan(after).any(|(one, value)| one.name == cell("APP_B").of && value == first));
        assert!(plan(CallerEnv::default()).next().is_none());
    }
}
