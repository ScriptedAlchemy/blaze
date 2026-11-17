//! Shared bundle fetches, their durability fences and immutable manifest bindings on Sim.

use hegel::{TestCase, generators as gs};

use super::*;
use crate::bundle::Plant;

fn require(ok: bool, why: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why.to_owned()) }
}

fn small() -> Vec<(Key, Kind, Vec<u8>)> {
    // alloc: three bounded fixture values, shared by the simulated owners.
    (0..3).map(|n| (key(n), Kind::Query, vec![n as u8; 17 + n as usize])).collect()
}

fn bundle(n: u8, entries: &[(Key, Kind, Vec<u8>)]) -> Manifest {
    let mut spec = spec();
    spec.settings = ContentId([n; 32]);
    Manifest::sign(
        BundleId::of(&spec),
        entries.iter().map(|(key, kind, bytes)| (public(*key, *kind), Record::of(*key, *kind, bytes))),
        &publisher(1),
    )
}

/// A different owner has already asked the store for this value. Its receive is real Sim
/// IO, left pending until the test's outside peer answers or hangs up.
struct Waiting(Conn);

impl blaze_cas::Remote for Waiting {
    fn fetch(&mut self, _: &Key, tag: Tag, effects: &mut Vec<Effect>) -> blaze_cas::Asked {
        effects.push(Effect::Recv { conn: self.0, most: 4096, into: Vec::new(), tag });
        blaze_cas::Asked::Sent
    }
}

fn waiting(world: &mut World) -> Waiting {
    let at = Address::Tcp("10.0.0.2:443".parse().unwrap());
    let tag = Tag(RAW + 99);
    world.effects.push(Effect::Listen { at: at.clone(), tag });
    world.run();
    let conn = world.sim.dial(&at).unwrap();
    let mut out = Vec::new(); // alloc: one bounded batch of simulated completions
    for _ in 0..8 {
        world.sim.poll(&mut out, blaze_core::wait::Deadline::NEVER);
        let mut accepted = false;
        for completion in out.drain(..) {
            if completion.tag == tag {
                assert_eq!(completion.done, Done::Accepted { conn });
                accepted = true;
            } else {
                world.dispatch(completion);
            }
        }
        if accepted {
            return Waiting(conn);
        }
    }
    panic!("the outside owner's simulated connection was not accepted");
}

fn observe(
    world: &mut World,
    downloads: &mut [Option<Download>; 2],
    seen: &mut Vec<(Key, Fetched)>,
    reverse: bool,
    consume: bool,
) {
    world.run();
    let fetched = std::mem::take(&mut world.fetched);
    for turn in 0..2 {
        let at = if reverse { 1 - turn } else { turn };
        if consume && turn == 1 {
            continue;
        }
        if let Some(download) = &mut downloads[at] {
            download.fetched(&fetched, &world.mine);
            download.fetched(&fetched, &world.mine); // A repeated notification must not count twice.
        }
    }
    seen.extend(fetched);
}

fn overlapping(
    seed: u64,
    prior: bool,
    ending: u8,
    reverse: bool,
    retire_first: bool,
    plant: Plant,
    consume: bool,
) -> Result<(), String> {
    let mut world = World::new(seed);
    let values = small();
    for (key, kind, bytes) in &values {
        world.seed(*key, *kind, bytes);
    }
    let first = bundle(1, &values[..2]);
    let second = bundle(2, &[values[0].clone(), values[2].clone()]);
    for manifest in [&first, &second] {
        world.client.publish_bundle(manifest.clone(), &mut world.effects);
    }
    world.run();
    require(world.published.iter().all(|it| it.result.is_ok()), "fixture manifests did not publish")?;

    let mut pending = prior.then(|| waiting(&mut world));
    if let Some(remote) = &mut pending {
        require(
            world.mine.fetch(&values[0].0, remote, crate::public::fetchable, &mut world.effects).is_some(),
            "the other owner did not start the shared fetch",
        )?;
    }
    let mut downloads = [
        Some(Download::start(first.id, &mut world.client, &mut world.effects)),
        Some(Download::start(second.id, &mut world.client, &mut world.effects)),
    ];
    for download in downloads.iter_mut().flatten() {
        download.plant = plant;
    }
    world.run();
    let mut got = std::mem::take(&mut world.bundles);
    for download in downloads.iter_mut().flatten() {
        got = download.bundles(got, &mut world.mine, &mut world.client, &mut world.effects);
    }
    require(got.is_empty(), "a manifest was not delivered to its owner")?;
    for download in downloads.iter().flatten() {
        require(download.done().is_none(), "a download completed before its missing values arrived")?;
    }
    if retire_first {
        downloads[0] = None; // Dropping one waiter must not cancel another owner's fetch.
    }
    let mut seen = Vec::new(); // alloc: the bounded transcript the independent oracle inspects
    observe(&mut world, &mut downloads, &mut seen, reverse, consume);
    if let Some(remote) = pending {
        for download in downloads.iter().flatten() {
            require(download.done().is_none(), "a download ignored the other owner's pending value")?;
        }
        require(!world.mine.has(&values[0].0), "the held value arrived before its peer answered")?;
        if ending == 3 {
            world.sim.peer_hangup(remote.0);
        } else {
            let (key, kind, bytes) = &values[0];
            let answer = if ending == 1 {
                blaze_cas::remote::Answer::Missing { key: *key }
            } else {
                let mut content = blaze_cas::content_of(bytes);
                if ending == 2 {
                    content.0[0] ^= 1;
                }
                blaze_cas::remote::Answer::Value { key: *key, kind: *kind, content, bytes: bytes.clone() }
            };
            world.sim.peer_send(remote.0, &answer.encode_framed());
        }
        observe(&mut world, &mut downloads, &mut seen, reverse, consume);
    }
    let outcome = seen.iter().find(|(key, _)| *key == values[0].0).map(|(_, how)| how);
    for (at, download) in downloads.iter_mut().enumerate() {
        let Some(download) = download else { continue };
        let before = download.done().cloned();
        if !prior || ending == 0 {
            let Some(Ok(Downloaded::Kept { listed, fetched, bytes, tickets })) = &before else {
                return Err("an overlapping download did not receive its shared completion".into());
            };
            let own = if at == 0 { &values[1] } else { &values[2] };
            require((*listed, *fetched, *bytes) == (2, 2, (values[0].2.len() + own.2.len()) as u64),
                "a shared or repeated completion changed the download's counts")?;
            let Some(Fetched::Kept(ticket)) = outcome else {
                return Err("the shared fetch supplied no durability ticket".into());
            };
            require(tickets.iter().filter(|kept| *kept == ticket).count() == 1,
                "a shared durability ticket was lost or counted twice")?;
            require(world.kept(&values[0].0).as_ref() == Some(&values[0].2),
                "a successful download lacks its shared value")?;
        } else {
            let Some(Err(why)) = &before else {
                return Err("a late refusal or missing value did not fail every waiting bundle".into());
            };
            require(why.contains(&format!("{:x}", values[0].0.0)), "a refusal omitted the shared key")?;
            if let Some(Fetched::Refused(cause)) = outcome {
                require(why.contains(cause), "the bundle replaced the store's original refusal")?;
            }
            require(!world.mine.has(&values[0].0), "a refused shared value entered the store")?;
        }
        download.fetched(&[(values[0].0, Fetched::Kept(blaze_cas::Ticket::DURABLE))], &world.mine);
        require(download.done().cloned() == before, "a late duplicate changed a terminal download")?;
    }
    Ok(())
}

fn publish_while_the_manifest_is_scheduled(store: &mut Store) {
    let values = small();
    let value = &values[0];
    let mut writer = store.writer();
    let guard = blaze_cas::enter();
    writer.put(&guard, value.0, value.1, &value.2).unwrap();
}

fn present(seed: u64, crash_before_sync: bool, racing: bool, plant: Plant) -> Result<(), String> {
    let mut world = World::new(seed);
    let values = small();
    let value = &values[0];
    world.seed(value.0, value.1, &value.2);
    let manifest = bundle(3, &values[..1]);
    world.client.publish_bundle(manifest.clone(), &mut world.effects);
    world.run();
    let pending = if racing {
        None
    } else {
        Some(world.mine.put(value.0, value.1, &value.2).unwrap())
    };
    let mut download = Download::start(manifest.id, &mut world.client, &mut world.effects);
    download.plant = plant;
    if racing {
        download.publish_during_manifest = Some(publish_while_the_manifest_is_scheduled);
    }
    world.run();
    let got = std::mem::take(&mut world.bundles);
    require(download.bundles(got, &mut world.mine, &mut world.client, &mut world.effects).is_empty(),
        "the already-readable bundle's manifest was not delivered")?;
    let Some(Ok(Downloaded::Kept { listed, fetched, bytes, tickets })) = download.done() else {
        return Err("the already-readable bundle did not settle".into());
    };
    require((*listed, *fetched, *bytes) == (1, 0, 0), "an already-readable value was fetched again")?;
    if let Some(pending) = pending {
        require(tickets.contains(&pending), "the already-readable put lost its durability fence")?;
    }
    require(tickets.len() == 1 && !world.mine.durable(tickets[0]),
        "the readable value lost its pending durability fence")?;
    world.sim.hold_syncs(FileName::new("mine").unwrap());
    world.mine.flush(&mut world.effects);
    world.run();
    require(world.sim.held_syncs() > 0, "the fixture did not hold a real store sync")?;
    require(tickets.iter().any(|ticket| !world.mine.durable(*ticket)),
        "the bundle claimed durability before the held sync")?;
    if !crash_before_sync {
        world.sim.release_held(Duration::from_micros(1));
        world.run();
        require(tickets.iter().all(|ticket| world.mine.durable(*ticket)),
            "the bundle's put did not become durable after its sync")?;
    }
    world.sim.crash();
    world.effects.clear();
    world.mine = Store::open(store_config("mine", MINE), &mut world.effects);
    world.run();
    require(world.mine.ready(), "the store failed to reopen after the simulated crash")?;
    let recovered = world.kept(&value.0);
    require(recovered.as_ref().is_none_or(|bytes| bytes == &value.2),
        "recovery exposed bytes other than the recorded value")?;
    if !crash_before_sync {
        require(recovered.as_ref() == Some(&value.2), "a durable bundle value was lost by the crash")?;
    }
    Ok(())
}

fn bindings(seed: u64, reverse: bool, forget: bool, partial: bool) -> Result<(), String> {
    let mut keys = [key(0), key(1)];
    keys.sort_unstable();
    let (fresh, common) = (keys[0], keys[1]);
    let (before, after) = if reverse { (2, 1) } else { (1, 2) };
    let first = bundle(4, &[(common, Kind::Query, vec![before])]);
    let second = bundle(5, &[(fresh, Kind::Query, vec![3]), (common, Kind::Query, vec![after])]);
    let old = first.entries[0];
    let prefix = second.entries[0];
    let first_id = first.id;
    let second_id = second.id;
    let mut answers = std::collections::VecDeque::from([first.clone(), first, second]);
    let mut world = World::raw(seed, move |path, _| {
        assert_eq!(path, client::BUNDLE_PATH);
        let manifest = answers.pop_front().expect("three bounded manifest requests");
        let mut body = Vec::new(); // alloc: one bounded simulated response
        wire::header(wire::BUNDLE_ANSWER, &mut body);
        body.push(wire::FOUND);
        manifest.encode(&mut body);
        send(response(200, &body))
    });
    for _ in 0..2 {
        world.client.bundle(first_id, &mut world.effects);
        world.run();
        let got = std::mem::take(&mut world.bundles);
        require(got.len() == 1 && got[0].result.is_ok(), "an equal manifest was refused")?;
    }
    if forget {
        world.client.forget_manifest_binding(&common);
    }
    if partial {
        world.client.bind_manifest_prefix(prefix);
    }
    world.client.bundle(second_id, &mut world.effects);
    world.run();
    let got = std::mem::take(&mut world.bundles);
    require(got.len() == 1, "the conflicting manifest did not have one answer")?;
    let Err(why) = &got[0].result else {
        return Err("a later manifest replaced an immutable key binding".into());
    };
    require(why.contains(&format!("{:x}", common.0)), "the conflict omitted its key")?;
    require(world.client.listed(&common) == Some(&old), "the original manifest binding changed")?;
    require(world.client.listed(&fresh).is_none(), "a refused manifest partly changed the binding map")?;
    Ok(())
}

/// The store checks a value against its own content hash, not against what a manifest
/// says belongs to its input key. Keep those contracts distinct: these bytes really are
/// valid in the store, even when their content, kind or length differs from the manifest.
fn records(seed: u64, source: u8, different: u8, plant: Plant) -> Result<(), String> {
    let values = small();
    let value = &values[0];
    let mut record = Record::of(value.0, value.1, &value.2);
    match different {
        0 => {}
        1 => {
            let mut other = value.2.clone(); // alloc: one bounded fixture value
            other[0] ^= 1;
            record.content = blaze_cas::content_of(&other);
        }
        2 => record.kind = Kind::Code,
        3 => record.len += 1,
        _ => unreachable!("four bounded record cases"),
    }
    let id = bundle(6, &values[..1]).id;
    let manifest = Manifest::sign(id, [(public(record.key, record.kind), record)], &publisher(1));
    require(manifest.verify(&trust()).is_ok(), "the record fixture's manifest is not verified")?;
    let mut world = World::raw(seed, move |path, _| {
        assert_eq!(path, client::BUNDLE_PATH);
        let mut body = Vec::new(); // alloc: one bounded simulated response
        wire::header(wire::BUNDLE_ANSWER, &mut body);
        body.push(wire::FOUND);
        manifest.encode(&mut body);
        send(response(200, &body))
    });
    let mut remote = (source == 2).then(|| waiting(&mut world));
    if let Some(remote) = &mut remote {
        require(
            world.mine.fetch(&value.0, remote, crate::public::fetchable, &mut world.effects).is_some(),
            "the independent remote did not start the shared fetch",
        )?;
    } else if source == 0 {
        world.mine.put(value.0, value.1, &value.2).unwrap();
    }
    let mut downloads = [
        Some(Download::start(id, &mut world.client, &mut world.effects)),
        Some(Download::start(id, &mut world.client, &mut world.effects)),
    ];
    for download in downloads.iter_mut().flatten() {
        download.plant = plant;
    }
    if source == 1 {
        downloads[0].as_mut().unwrap().publish_during_manifest = Some(publish_while_the_manifest_is_scheduled);
    }
    world.run();
    let mut got = std::mem::take(&mut world.bundles);
    for download in downloads.iter_mut().flatten() {
        got = download.bundles(got, &mut world.mine, &mut world.client, &mut world.effects);
    }
    require(got.is_empty(), "a record fixture's manifest was not delivered")?;
    if let Some(remote) = remote {
        for download in downloads.iter().flatten() {
            require(download.done().is_none(), "a shared value settled before its remote answered")?;
        }
        let answer = blaze_cas::remote::Answer::Value {
            key: value.0,
            kind: value.1,
            content: blaze_cas::content_of(&value.2),
            bytes: value.2.clone(), // alloc: the independent remote's bounded response
        };
        world.sim.peer_send(remote.0, &answer.encode_framed());
    }
    let mut seen = Vec::new(); // alloc: the bounded transcript the independent oracle inspects
    observe(&mut world, &mut downloads, &mut seen, seed & 1 != 0, false);
    if source == 2 {
        require(seen.len() == 1 && matches!(&seen[0].1, Fetched::Kept(_)),
            "the independent remote's self-consistent value was not actually kept by the store")?;
    } else {
        require(seen.is_empty(), "an already-readable value was fetched again")?;
    }
    let actual = world.mine.get(&value.0).map_err(|why| why.to_string())?
        .ok_or("the self-consistent value is not readable")?;
    require(actual.kind == value.1 && actual.bytes == value.2.as_slice(),
        "a refused bundle replaced or removed another owner's valid store value")?;
    require(downloads[0].as_ref().unwrap().done() == downloads[1].as_ref().unwrap().done(),
        "owners of the same manifest disagreed about the same stored value")?;
    for download in downloads.iter_mut().flatten() {
        let before = download.done().cloned();
        if different == 0 {
            let Some(Ok(Downloaded::Kept { listed, fetched, bytes, tickets })) = &before else {
                return Err("a value matching its manifest was not kept".into());
            };
            let count = usize::from(source == 2);
            require((*listed, *fetched, *bytes) == (1, count, (value.2.len() * count) as u64),
                "a matching value was counted incorrectly")?;
            require(tickets.len() == 1, "a matching value lost or repeated its pending ticket")?;
        } else {
            let Some(Err(why)) = &before else {
                return Err("a bundle accepted a value from another manifest record".into());
            };
            require(why.contains(&format!("{:x}", value.0.0)) && why.contains("record"),
                "a manifest mismatch omitted its key or cause")?;
        }
        download.fetched(&[(value.0, Fetched::Kept(blaze_cas::Ticket::DURABLE))], &world.mine);
        require(download.done().cloned() == before, "a repeated outcome changed a settled record check")?;
    }
    Ok(())
}

/// A previous attempt's outcome and a new manifest are queued in the same driver turn.
/// The next attempt really succeeds, so only reversing delivery order can attribute the
/// earlier missing/corrupt/disconnected attempt to the new download.
fn retry(seed: u64, ending: u8, after_manifest: bool) -> Result<(), String> {
    let mut world = World::new(seed);
    let values = small();
    let value = &values[0];
    world.seed(value.0, value.1, &value.2);
    let manifest = bundle(7, &values[..1]);
    world.client.publish_bundle(manifest.clone(), &mut world.effects);
    world.run();
    let mut remote = waiting(&mut world);
    require(world.mine.fetch(&value.0, &mut remote, crate::public::fetchable, &mut world.effects).is_some(),
        "the previous generation did not start its fetch")?;
    if ending == 3 {
        world.sim.peer_hangup(remote.0);
    } else {
        let answer = if ending == 1 {
            blaze_cas::remote::Answer::Missing { key: value.0 }
        } else {
            let mut content = blaze_cas::content_of(&value.2);
            if ending == 2 {
                content.0[0] ^= 1;
            }
            blaze_cas::remote::Answer::Value {
                key: value.0, kind: value.1, content, bytes: value.2.clone(),
            }
        };
        world.sim.peer_send(remote.0, &answer.encode_framed());
    }
    world.run();
    let queued = std::mem::take(&mut world.fetched);
    require(queued.len() == 1 && queued[0].0 == value.0, "the earlier fetch has no queued outcome")?;
    require(matches!(&queued[0].1, Fetched::Kept(_)) == (ending == 0),
        "the earlier fetch did not reach its intended outcome")?;
    let download = Download::start(manifest.id, &mut world.client, &mut world.effects);
    world.run();
    let manifests = std::mem::take(&mut world.bundles);
    require(manifests.len() == 1, "the new manifest did not arrive beside the queued outcome")?;
    world.download = Some(download);
    world.gathered(queued, manifests, after_manifest);
    world.run();
    require(world.kept(&value.0).as_ref() == Some(&value.2),
        "the current attempt did not make the fixture's value readable")?;
    if ending != 0 {
        require(world.fetched.len() == 2 && matches!(&world.fetched[1].1, Fetched::Kept(_)),
            "the retry did not produce its own successful store outcome")?;
    }
    let Some(Ok(Downloaded::Kept { listed, fetched, bytes, tickets })) = world.download.as_ref().unwrap().done() else {
        return Err("an old fetch outcome settled a bundle that started a new attempt".into());
    };
    let count = usize::from(ending != 0);
    require((*listed, *fetched, *bytes) == (1, count, (value.2.len() * count) as u64),
        "the new manifest counted an earlier attempt as its own")?;
    require(tickets.len() == 1, "the new attempt lost or repeated its own durability fence")?;
    Ok(())
}

/// The invariant: every overlapping bundle waits for all its missing values, including a
/// fetch started by another owner, and receives the same original refusal or durability
/// ticket exactly once. Dropping a waiter and repeating notifications do not change it;
/// a queued outcome is broadcast before a new manifest can retry the same key.
#[hegel::test(test_cases = 64)]
fn overlapping_downloads_wait_for_shared_fetches_and_keep_their_outcomes(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let prior = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    let ending = tc.draw(gs::integers::<u8>().max_value(3));
    let reverse = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    let retire = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    overlapping(seed, prior, ending, reverse, retire, Plant::None, false).unwrap();
    retry(seed, ending, false).unwrap();
}

/// The invariant: a bundle's durability receipt covers already-readable pending puts as
/// well as fetched values; no held sync counts as durable, and acknowledged values survive
/// a crash and reopen unchanged.
#[hegel::test(test_cases = 32)]
fn readable_bundle_values_keep_their_pending_durability_fence(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let before = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    let racing = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    present(seed, before, racing, Plant::None).unwrap();
}

/// The invariant: verified manifests may repeat an equal key binding but never replace
/// it; refusing one conflicting record changes none of that manifest's other bindings.
#[hegel::test(test_cases = 32)]
fn verified_manifests_keep_immutable_bindings_and_refuse_conflicts_atomically(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let reverse = tc.draw(gs::integers::<u8>().max_value(1)) != 0;
    bindings(seed, reverse, false, false).unwrap();
}

/// The invariant: a bundle accepts only its signed content, kind and length, including a
/// local value, a writer publishing during scheduling and a fetch another remote owns.
/// A different self-consistent store value stays available to its owner but fails the bundle.
#[hegel::test(test_cases = 64)]
fn bundle_values_keep_their_manifest_bindings_across_local_and_shared_sources(tc: TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let source = tc.draw(gs::integers::<u8>().max_value(2));
    let different = tc.draw(gs::integers::<u8>().max_value(3));
    records(seed, source, different, Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_a_download_ignoring_an_existing_fetch() {
    overlapping(40, true, 0, false, false, Plant::None, false).unwrap();
    assert!(overlapping(40, true, 0, false, false, Plant::IgnoreShared, false).is_err());
}

#[test]
fn the_oracle_catches_a_shared_completion_consumed_by_one_download() {
    overlapping(41, false, 0, false, false, Plant::None, false).unwrap();
    assert!(overlapping(41, false, 0, false, false, Plant::None, true).is_err());
}

#[test]
fn the_oracle_catches_a_readable_value_without_its_durability_fence() {
    present(42, true, false, Plant::None).unwrap();
    assert!(present(42, true, false, Plant::ForgetPresentTickets).is_err());
}

#[test]
fn the_oracle_catches_waiting_for_a_value_published_during_fetch_scheduling() {
    present(44, false, true, Plant::None).unwrap();
    assert!(present(44, false, true, Plant::IgnoreNewlyReadable).is_err());
}

#[test]
fn the_oracle_catches_a_lost_manifest_binding() {
    bindings(43, false, false, false).unwrap();
    assert!(bindings(43, false, true, false).is_err());
}

#[test]
fn the_oracle_catches_a_partly_applied_conflicting_manifest() {
    bindings(45, false, false, false).unwrap();
    assert!(bindings(45, false, false, true).is_err());
}

#[test]
fn the_oracle_catches_a_readable_value_from_another_manifest_record() {
    for different in 1..=3 {
        records(46, 0, different, Plant::None).unwrap();
        assert!(records(46, 0, different, Plant::IgnorePresentRecord).is_err());
    }
}

#[test]
fn the_oracle_catches_a_racing_value_from_another_manifest_record() {
    for different in 1..=3 {
        records(47, 1, different, Plant::None).unwrap();
        assert!(records(47, 1, different, Plant::IgnorePresentRecord).is_err());
    }
}

#[test]
fn the_oracle_catches_a_shared_fetch_from_another_manifest_record() {
    for different in 1..=3 {
        records(48, 2, different, Plant::None).unwrap();
        assert!(records(48, 2, different, Plant::IgnoreFetchedRecord).is_err());
    }
}

#[test]
fn the_oracle_catches_an_old_fetch_outcome_delivered_after_a_new_manifest() {
    for ending in 1..=3 {
        retry(49, ending, false).unwrap();
        assert!(retry(49, ending, true).is_err());
    }
}
