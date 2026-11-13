//! Interpret the production whole-write SQEs on Sim's disk. Small import limits reproduce
//! Linux's MAX_RW_COUNT boundary without allocating or writing gigabytes.

use crate::{
    run_until_idle,
    sim::{Faults, Rng, Sim},
};
use hegel::generators as gs;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Write,
    Zero,
    FileSync,
    Rename,
    DirSync,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    seed: u64,
    len: usize,
    import_limit: u32,
    short: bool,
    fault: Fault,
    crash_after: usize,
}

fn name(s: &str) -> FileName {
    FileName::new(s).unwrap()
}

fn io(sim: &mut Sim, effect: Effect) -> Done {
    let mut answer = None;
    run_until_idle(sim, &mut vec![effect], |it, _| answer = Some(it.done))
        .expect("one simulated syscall ends");
    answer.expect("one syscall completion")
}

/// The same transition and SQEs as Uring::wrote/path_sqes. Only kernel execution is replaced.
fn simulate(case: Case, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(case.seed, Faults::default());
    let mut rng = Rng::new(case.seed ^ 0xc10e);
    let (temp, target) = (name("placed/temporary"), name("placed/value"));
    let old: &[u8] = b"the old output";
    sim.change(&target, old);
    let bytes: Vec<u8> = (0..case.len).map(|n| n as u8 ^ 0xa5).collect();
    let mut hot = Hot::new(Tag(1), Kind::Write, 0);
    let mut cold = Cold { slots: [0, 1], ..Cold::EMPTY };
    let (mut ops, mut synced, mut injected) = (0, false, false);
    let names = [c"temporary".as_ptr().cast(), c"value".as_ptr().cast(), c"placed".as_ptr().cast()];
    for _ in 0..1024 {
        let mut sqes = [Sqe::default(); LINKS];
        let mut n = whole::sqes(&hot, &cold, &bytes, names, |n| n as u64, &mut sqes);
        // Plant the old missing gate: publish in the write's chain, before its count is known.
        // The suffix itself comes from the production commit SQE builder, not a second writer.
        if plant && matches!(hot.phase, OPENING | OPENED) {
            sqes[n - 1].flags |= SQE_IO_LINK;
            let mut commit = [Sqe::default(); LINKS];
            let committing = Hot { phase: COMMITTING, ..hot };
            let more = whole::sqes(&committing, &cold, &bytes, names, |n| n as u64, &mut commit);
            sqes[n..n + more].copy_from_slice(&commit[..more]);
            n += more;
        }
        let mut completions = Vec::new();
        let mut cancelled = false;
        for sqe in &sqes[..n] {
            let link = sqe.user_data as usize;
            let wanted_fault = match sqe.opcode {
                OP_WRITE => {
                    case.fault == Fault::Write || (case.fault == Fault::Zero && sqe.len > 0)
                }
                OP_FSYNC if link == 3 => case.fault == Fault::FileSync,
                OP_RENAMEAT => case.fault == Fault::Rename,
                OP_FSYNC if link == 5 => case.fault == Fault::DirSync,
                _ => false,
            };
            let res = if cancelled {
                -libc::ECANCELED
            } else if wanted_fault && !injected {
                injected = true;
                if case.fault == Fault::Zero { 0 } else { -libc::EIO }
            } else {
                match sqe.opcode {
                    OP_OPENAT if link == 0 => 0,
                    OP_OPENAT => {
                        assert_eq!(
                            io(
                                &mut sim,
                                Effect::Write {
                                    file: temp.clone(),
                                    bytes: Vec::new(),
                                    tag: Tag(1),
                                }
                            ),
                            Done::Written
                        );
                        0
                    }
                    OP_WRITE => {
                        let at = sqe.off as usize;
                        assert_eq!(sqe.addr, bytes[at..].as_ptr() as u64);
                        let imported = sqe.len.min(case.import_limit);
                        let put = if case.short && imported > 1 { imported / 2 } else { imported };
                        let done = io(
                            &mut sim,
                            Effect::WriteAt {
                                file: temp.clone(),
                                at: sqe.off,
                                bytes: bytes[at..at + put as usize].to_vec(),
                                tag: Tag(1),
                            },
                        );
                        assert_eq!(done, Done::Written);
                        synced = false;
                        put as i32
                    }
                    OP_FSYNC if link == 3 => {
                        assert_eq!(
                            io(&mut sim, Effect::Sync { file: temp.clone(), tag: Tag(1) }),
                            Done::Synced
                        );
                        synced = true;
                        0
                    }
                    OP_RENAMEAT => {
                        // Publication's invariant, checked BEFORE Sim's completed-rename
                        // durability can hide a missing sync or an early publication.
                        if !synced || sim.contents(&temp) != bytes {
                            return Err(
                                "rename published before all bytes were written and synced".into()
                            );
                        }
                        assert_eq!(
                            io(
                                &mut sim,
                                Effect::Rename {
                                    from: temp.clone(),
                                    to: target.clone(),
                                    tag: Tag(1),
                                }
                            ),
                            Done::Renamed
                        );
                        0
                    }
                    OP_FSYNC => 0, // Sim makes a completed rename durable; model dir errors above.
                    OP_UNLINKAT => {
                        assert_eq!(
                            io(&mut sim, Effect::Remove { file: temp.clone(), tag: Tag(1) }),
                            Done::Removed
                        );
                        0
                    }
                    other => panic!("unexpected opcode {other}"),
                }
            };
            // An imported prefix can succeed as a linked write. An ordinary short write
            // cancels the suffix, as Linux does, relative to the IMPORTED count.
            cancelled |= sqe.flags & SQE_IO_LINK != 0
                && (res < 0
                    || (sqe.opcode == OP_WRITE && res < sqe.len.min(case.import_limit) as i32));
            completions.push((link, res));
            ops += 1;
            let visible = sim.contents(&target);
            if visible != old && visible != bytes {
                return Err("the canonical name contains a partial output".into());
            }
            if ops == case.crash_after {
                sim.crash();
                let visible = sim.contents(&target);
                return if visible == old || visible == bytes {
                    Ok(())
                } else {
                    Err("a crash exposed partial canonical bytes".into())
                };
            }
        }
        rng.shuffle(&mut completions);
        let mut results = [0; LINKS];
        for (link, res) in completions {
            results[link] = res;
        }
        match whole::advance(&mut hot, &mut cold, bytes.len(), results) {
            Step::Again => {}
            Step::Ends(Some(Done::Written)) => {
                if injected {
                    return Err("an injected error was reported as success".into());
                }
                sim.crash();
                return if sim.contents(&target) == bytes && !sim.exists(&temp) {
                    Ok(())
                } else {
                    Err("success did not leave exactly the durable output".into())
                };
            }
            Step::Ends(Some(Done::Failed(_))) if injected => {
                sim.crash();
                let visible = sim.contents(&target);
                return if !sim.exists(&temp) && (visible == old || visible == bytes) {
                    Ok(())
                } else {
                    Err("failure left temporary or partial canonical bytes".into())
                };
            }
            _ => return Err(format!("unexpected write outcome in phase {}", hot.phase)),
        }
    }
    Err("whole-file write exceeded its positive-progress bound".into())
}

#[hegel::test(test_cases = 128)]
fn whole_write_publication_survives_import_limits_faults_and_crashes(tc: hegel::TestCase) {
    let case = Case {
        seed: tc.draw(gs::integers()),
        len: tc.draw(gs::integers::<u16>().max_value(128)) as usize,
        import_limit: tc.draw(gs::integers().min_value(1).max_value(32)),
        short: tc.draw(gs::booleans()),
        fault: [
            Fault::None,
            Fault::Write,
            Fault::Zero,
            Fault::FileSync,
            Fault::Rename,
            Fault::DirSync,
        ][tc.draw(gs::integers::<u8>().max_value(5)) as usize],
        crash_after: tc.draw(gs::integers::<u16>().max_value(64)) as usize,
    };
    assert_eq!(simulate(case, false), Ok(()), "{case:?}");
}

#[test]
fn whole_write_oracle_catches_publication_linked_to_a_clamped_write() {
    let case = Case {
        seed: 94,
        len: 43,
        import_limit: 32,
        short: false,
        fault: Fault::None,
        crash_after: 0,
    };
    assert_eq!(simulate(case, false), Ok(()));
    assert_eq!(
        simulate(case, true),
        Err("rename published before all bytes were written and synced".into())
    );
    for fault in
        [Fault::None, Fault::Write, Fault::Zero, Fault::FileSync, Fault::Rename, Fault::DirSync]
    {
        assert_eq!(simulate(Case { fault, ..case }, false), Ok(()), "{fault:?}");
    }
    for crash_after in 1..=7 {
        assert_eq!(simulate(Case { crash_after, ..case }, false), Ok(()), "crash {crash_after}");
    }
}

#[test]
fn whole_write_counts_past_linux_max_rw_count_before_committing() {
    let len = 2_739_161_264;
    let mut hot = Hot::new(Tag(1), Kind::Write, 0);
    let mut cold = Cold::EMPTY;
    let mut results = [0; LINKS];
    results[2] = 0x7fff_f000; // Linux's 4 KiB-page MAX_RW_COUNT, a successful imported prefix.
    assert!(matches!(whole::advance(&mut hot, &mut cold, len, results), Step::Again));
    assert_eq!((hot.done, hot.phase), (0x7fff_f000, OPENED));
    results[2] = (len as u64 - hot.done) as i32;
    assert!(matches!(whole::advance(&mut hot, &mut cold, len, results), Step::Again));
    assert_eq!((hot.done, hot.phase), (len as u64, COMMITTING));
    assert!(matches!(
        whole::advance(&mut hot, &mut cold, len, [0; LINKS]),
        Step::Ends(Some(Done::Written))
    ));
}
