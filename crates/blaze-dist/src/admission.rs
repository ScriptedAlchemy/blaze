//! Section 5's admission: which job runs, where, in which order, within which capacity.
//!
//! Each machine declares a capacity (cores and memory, less what is held back). Every executor
//! holds a [`Reservation`] on its machine from the moment it is created to the moment it
//! stops, and no machine's reservations ever sum past its capacity. Jobs wait in one queue per
//! platform: `Main` before pull requests, then first in, first out, with no job overtaking the
//! head of its class. Executors are matched by platform, lifecycle namespace and reservation,
//! not a new application trust policy: access remains the deployment's existing ACLs.
//! An executor's rebuild starts
//! beside the old one only if both fit; otherwise the old one
//! drains first. Past `Q` waiting jobs a platform answers `busy`.

use std::alloc::{Allocator, Global};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;

use blaze_wire::{Attempt, Digest, ExecutionId, Namespace, Reservation};

use crate::{Millis, Refusal};

/// A machine's held reservations exceed the capacity that it advertised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityExceeded<'a> {
    /// The machine whose capacity was exceeded.
    pub machine: &'a str,
    /// The reservations held by the machine's executors.
    pub used: Reservation,
    /// The capacity originally advertised by this machine.
    pub capacity: &'a Reservation,
}

impl std::fmt::Display for CapacityExceeded<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "machine {} holds {:?}, past its capacity {:?}", self.machine, self.used, self.capacity)
    }
}

impl std::error::Error for CapacityExceeded<'_> {}

/// Admission's bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// `Q`: the most executions waiting per platform.
    pub queue_limit: usize,
    /// What a `busy` answer tells the requester to wait.
    pub busy_retry_ms: u64,
    /// A pull request's executor idle this long is destroyed (24 h).
    pub pr_idle_ms: u64,
}

impl Default for AdmissionConfig {
    fn default() -> AdmissionConfig {
        AdmissionConfig { queue_limit: 32, busy_retry_ms: 30_000, pr_idle_ms: 24 * 3_600_000 }
    }
}

/// A job's class in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// `Main`: trusted builders only, admitted first.
    Main,
    /// A pull request's.
    Pr,
}

impl Class {
    /// The class of a namespace's jobs.
    pub fn of(namespace: &Namespace) -> Class {
        match namespace {
            Namespace::Main => Class::Main,
            Namespace::Pr(_) => Class::Pr,
        }
    }
}

/// A job waiting for admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The execution.
    pub execution: ExecutionId,
    /// Its namespace, which says which executors may run it.
    pub namespace: Namespace,
    /// Its platform's digest.
    pub platform: Digest,
    /// What its lead executor reserves when one is created for it.
    pub need: Reservation,
    /// When it was queued.
    pub at: Millis,
}

/// Where an executor is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutorPhase {
    /// Takes jobs.
    Ready,
    /// Finishes its job and takes no other: a rebuild waits for it.
    Draining,
    /// Gone; its reservation is free.
    Stopped,
}

/// One executor, as admission holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorSlot {
    /// Its id.
    pub id: String,
    /// The machine it runs on.
    pub machine: String,
    /// The namespace that created the slot, for lifecycle bookkeeping, not an access boundary.
    pub namespace: Namespace,
    /// What it holds on its machine.
    pub reservation: Reservation,
    /// The execution and attempt it runs. Completion acknowledgements must match both.
    pub busy: Option<(ExecutionId, Attempt)>,
    /// Where it is.
    pub phase: ExecutorPhase,
    /// When it last finished a job.
    pub idle_since: Millis,
}

/// A job let in, and the executor it runs on.
pub struct Admitted<A: Allocator> {
    /// The execution.
    pub execution: ExecutionId,
    /// Its namespace.
    pub namespace: Namespace,
    /// Its platform.
    pub platform: Digest,
    /// The lead executor's name, held in the caller's allocator.
    pub executor: Box<str, A>,
    /// Whether the executor was created for it.
    pub created: bool,
}

impl<A: Allocator + Clone> Clone for Admitted<A> {
    fn clone(&self) -> Self {
        Self {
            execution: self.execution,
            namespace: self.namespace.clone(),
            platform: self.platform,
            executor: self.executor.clone(),
            created: self.created,
        }
    }
}

impl<A: Allocator> std::fmt::Debug for Admitted<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admitted")
            .field("execution", &self.execution)
            .field("namespace", &self.namespace)
            .field("platform", &self.platform)
            .field("executor", &self.executor)
            .field("created", &self.created)
            .finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Admitted<B>> for Admitted<A> {
    fn eq(&self, other: &Admitted<B>) -> bool {
        self.execution == other.execution
            && self.namespace == other.namespace
            && self.platform == other.platform
            && self.executor.as_ref() == other.executor.as_ref()
            && self.created == other.created
    }
}

impl<A: Allocator> Eq for Admitted<A> {}

#[derive(Clone, Debug)]
struct Queue {
    main: VecDeque<Waiting>,
    pr: VecDeque<Waiting>,
}

impl Default for Queue {
    fn default() -> Self {
        Self { main: VecDeque::new_in(Global), pr: VecDeque::new_in(Global) }
    }
}

#[derive(Clone, Debug)]
struct Machine {
    platform: Digest,
    capacity: Reservation,
}

/// Admission over a fleet's machines.
#[derive(Clone, Debug)]
pub struct Admission {
    config: AdmissionConfig,
    machines: BTreeMap<String, Machine>,
    executors: BTreeMap<String, ExecutorSlot>,
    queues: BTreeMap<Digest, Queue>,
    /// Rebuilds that wait for their old builder to drain: the old one and the new reservation.
    rebuilds: BTreeMap<String, Reservation>,
    next_executor: u64,
}

fn fits(used: &Reservation, add: &Reservation, capacity: &Reservation) -> bool {
    used.memory.checked_add(add.memory).is_some_and(|m| m <= capacity.memory)
        && used.cpus.checked_add(add.cpus).is_some_and(|c| c <= capacity.cpus)
        && used.disk.checked_add(add.disk).is_some_and(|d| d <= capacity.disk)
}

/// Whether an executor that reserved `held` can run a job that needs `need`.
fn covers(held: &Reservation, need: &Reservation) -> bool {
    held.memory >= need.memory && held.cpus >= need.cpus && held.disk >= need.disk
}

impl Admission {
    /// No machine yet.
    pub fn new(config: AdmissionConfig) -> Admission {
        Admission {
            config,
            machines: BTreeMap::new_in(Global),
            executors: BTreeMap::new_in(Global),
            queues: BTreeMap::new_in(Global),
            rebuilds: BTreeMap::new_in(Global),
            next_executor: 1,
        }
    }

    /// A machine and its declared capacity.
    pub fn add_machine(&mut self, name: &str, platform: Digest, capacity: Reservation) {
        self.machines.insert(Box::clone_from_ref_in(name, Global).into_string(), Machine { platform, capacity });
    }

    /// The capacity of a machine.
    pub fn capacity(&self, machine: &str) -> Option<&Reservation> {
        self.machines.get(machine).map(|m| &m.capacity)
    }

    /// The execution platform served by a machine.
    pub fn platform(&self, machine: &str) -> Option<Digest> {
        self.machines.get(machine).map(|m| m.platform)
    }

    /// What the executors on a machine hold now.
    pub fn used(&self, machine: &str) -> Reservation {
        let mut used = Reservation { memory: 0, cpus: 0, disk: 0 };
        for e in self.executors.values() {
            if e.machine == machine && e.phase != ExecutorPhase::Stopped {
                used.memory += e.reservation.memory;
                used.cpus += e.reservation.cpus;
                used.disk += e.reservation.disk;
            }
        }
        used
    }

    /// The machines' names.
    pub fn machines(&self) -> impl Iterator<Item = &str> {
        self.machines.keys().map(String::as_str)
    }

    /// The executors.
    pub fn executors(&self) -> impl Iterator<Item = &ExecutorSlot> {
        self.executors.values()
    }

    /// One executor.
    pub fn executor(&self, id: &str) -> Option<&ExecutorSlot> {
        self.executors.get(id)
    }

    /// How many executions wait on a platform.
    pub fn waiting(&self, platform: &Digest) -> usize {
        self.queues.get(platform).map_or(0, |q| q.main.len() + q.pr.len())
    }

    fn create(
        &mut self,
        now: Millis,
        machine: &str,
        namespace: Namespace,
        reservation: Reservation,
    ) -> Option<&str> {
        let m = self.machines.get(machine)?;
        if !fits(&self.used(machine), &reservation, &m.capacity) {
            return None;
        }
        let mut bytes = Vec::new_in(Global);
        write!(&mut bytes, "exec:{}", self.next_executor).expect("writing an executor name to a vector");
        let id = String::from_utf8(bytes).expect("executor names contain only ASCII");
        self.next_executor += 1;
        let slot = self.executors
            .entry(Box::clone_from_ref_in(id.as_str(), Global).into_string())
            .insert_entry(ExecutorSlot {
                id,
                machine: Box::clone_from_ref_in(machine, Global).into_string(),
                namespace,
                reservation,
                busy: None,
                phase: ExecutorPhase::Ready,
                idle_since: now,
            })
            .into_mut();
        Some(slot.id.as_str())
    }

    /// A builder on `machine`, as the operator starts one: only if it fits. Its returned
    /// name belongs to `alloc`; the fleet retains its own name.
    pub fn add_builder<A: Allocator>(
        &mut self,
        now: Millis,
        machine: &str,
        reservation: Reservation,
        alloc: A,
    ) -> Option<Box<str, A>> {
        self.create(now, machine, Namespace::Main, reservation)
            .map(|id| Box::clone_from_ref_in(id, alloc))
    }

    /// Queues a job, or answers `busy` past `Q`.
    pub fn enqueue(&mut self, waiting: Waiting) -> Result<(), (Refusal, u64)> {
        let class = Class::of(&waiting.namespace);
        let queue = self.queues.entry(waiting.platform).or_default();
        if queue.main.len() + queue.pr.len() >= self.config.queue_limit {
            return Err((Refusal::Busy, self.config.busy_retry_ms));
        }
        match class {
            Class::Main => queue.main.push_back(waiting),
            Class::Pr => queue.pr.push_back(waiting),
        }
        Ok(())
    }

    /// Takes a waiting execution out of its queue: it was cancelled before it ran.
    pub fn dequeue(&mut self, execution: ExecutionId) -> bool {
        for queue in self.queues.values_mut() {
            for list in [&mut queue.main, &mut queue.pr] {
                if let Some(at) = list.iter().position(|w| w.execution == execution) {
                    list.remove(at);
                    return true;
                }
            }
        }
        false
    }

    /// Lets in what fits, in order: per platform, the head of `Main`'s queue first, and a pull
    /// request's job only while no `Main` job waits. A head that cannot run blocks its class.
    /// Returned names and transient lists use `alloc`.
    pub fn admit<A: Allocator + Clone>(&mut self, now: Millis, alloc: A) -> Vec<Admitted<A>, A> {
        // A rebuild that waits for room tries again whenever admission runs.
        let mut rebuilds = Vec::new_in(alloc.clone());
        rebuilds.extend(self.rebuilds.keys().map(|old| Box::clone_from_ref_in(old.as_str(), alloc.clone())));
        for old in rebuilds {
            self.start_rebuild(now, &old);
        }
        let mut admitted = Vec::new_in(alloc.clone());
        let mut platforms = Vec::new_in(alloc.clone());
        platforms.extend(self.queues.keys().copied());
        for platform in platforms {
            loop {
                let queue = &self.queues[&platform];
                let head = match queue.main.front().or(queue.pr.front()) {
                    Some(head) => head.clone(),
                    None => break,
                };
                let Some((executor, created)) = self.place(now, &head, alloc.clone()) else { break };
                let queue = self.queues.get_mut(&platform).expect("platform");
                match Class::of(&head.namespace) {
                    Class::Main => queue.main.pop_front(),
                    Class::Pr => queue.pr.pop_front(),
                };
                self.executors.get_mut(executor.as_ref()).expect("placed").busy =
                    Some((head.execution, Attempt(1)));
                admitted.push(Admitted {
                    execution: head.execution,
                    namespace: head.namespace,
                    platform,
                    executor,
                    created,
                });
            }
        }
        admitted
    }

    /// An executor for `job`: an idle one of its lifecycle namespace on its platform, or, for a pull
    /// request, a new one where its reservation fits.
    fn place<A: Allocator + Clone>(&mut self, now: Millis, job: &Waiting, alloc: A) -> Option<(Box<str, A>, bool)> {
        // An idle executor runs it only within what it reserved: a job never runs past its
        // executor's share of the machine.
        let idle = self.executors.values().find(|e| {
            e.phase == ExecutorPhase::Ready
                && e.busy.is_none()
                && e.namespace == job.namespace
                && covers(&e.reservation, &job.need)
                && self.machines.get(&e.machine).is_some_and(|m| m.platform == job.platform)
        });
        if let Some(e) = idle {
            return Some((Box::clone_from_ref_in(e.id.as_str(), alloc), false));
        }
        if job.namespace == Namespace::Main {
            // Persistent builders are started by the operator, never on demand for a job.
            return None;
        }
        let mut machines = Vec::new_in(alloc.clone());
        machines.extend(self.machines.iter()
            .filter(|(_, m)| m.platform == job.platform)
            .map(|(name, _)| Box::clone_from_ref_in(name.as_str(), alloc.clone())));
        machines
            .iter()
            .find_map(|machine| self.create(now, machine, job.namespace.clone(), job.need.clone())
                .map(|id| Box::clone_from_ref_in(id, alloc.clone())))
            .map(|id| (id, true))
    }

    /// Another idle executor of `namespace` on `platform` that covers `need`, for a
    /// reassignment, not one of `except`.
    pub fn idle_for(
        &self,
        namespace: &Namespace,
        platform: &Digest,
        need: &Reservation,
        except: &[&str],
    ) -> Option<&str> {
        self.executors
            .values()
            .find(|e| {
                e.phase == ExecutorPhase::Ready
                    && e.busy.is_none()
                    && e.namespace == *namespace
                    && covers(&e.reservation, need)
                    && !except.contains(&e.id.as_str())
                    && self.machines.get(&e.machine).is_some_and(|m| m.platform == *platform)
            })
            .map(|e| e.id.as_str())
    }

    /// Marks `executor` as running `execution`, for a reassignment.
    pub fn take(&mut self, executor: &str, execution: ExecutionId, attempt: Attempt) {
        if let Some(e) = self.executors.get_mut(executor) {
            e.busy = Some((execution, attempt));
        }
    }

    /// The assignment on `executor` ended: it is idle, or, draining, stops and lets its rebuild
    /// in. A stale acknowledgement does not change a newer assignment.
    pub fn finished(
        &mut self,
        now: Millis,
        executor: &str,
        execution: ExecutionId,
        attempt: Attempt,
    ) {
        let Some(e) = self.executors.get_mut(executor) else { return };
        if e.busy != Some((execution, attempt)) {
            return;
        }
        e.busy = None;
        e.idle_since = now;
        if e.phase == ExecutorPhase::Draining {
            e.phase = ExecutorPhase::Stopped;
            self.start_rebuild(now, executor);
        }
    }

    /// The executor died or was killed: its reservation is free.
    pub fn lost(&mut self, now: Millis, executor: &str) {
        if let Some(e) = self.executors.get_mut(executor) {
            e.phase = ExecutorPhase::Stopped;
            e.busy = None;
            self.start_rebuild(now, executor);
        }
    }

    /// The pull request closed: its executors are destroyed. Answers the ones that were busy,
    /// whose jobs end, with the result and its names held in `alloc`.
    pub fn close_pr<A: Allocator + Clone>(&mut self, n: u64, alloc: A) -> Vec<(Box<str, A>, Option<(ExecutionId, Attempt)>), A> {
        let mut gone = Vec::new_in(alloc.clone());
        for e in self.executors.values_mut() {
            if e.namespace == Namespace::Pr(blaze_wire::PrNamespace { n })
                && e.phase != ExecutorPhase::Stopped
            {
                e.phase = ExecutorPhase::Stopped;
                gone.push((Box::clone_from_ref_in(e.id.as_str(), alloc.clone()), e.busy.take()));
            }
        }
        gone
    }

    /// Destroys pull-request executors idle past the bound, returning their names in `alloc`.
    pub fn reap<A: Allocator + Clone>(&mut self, now: Millis, alloc: A) -> Vec<Box<str, A>, A> {
        let bound = self.config.pr_idle_ms;
        let mut gone = Vec::new_in(alloc.clone());
        for e in self.executors.values_mut() {
            if matches!(e.namespace, Namespace::Pr(_))
                && e.phase == ExecutorPhase::Ready
                && e.busy.is_none()
                && now > e.idle_since + bound
            {
                e.phase = ExecutorPhase::Stopped;
                gone.push(Box::clone_from_ref_in(e.id.as_str(), alloc.clone()));
            }
        }
        gone
    }

    /// A trusted builder's rebuild (a new identity, a new front end, growth, its job count):
    /// the new one starts beside the old only if both fit; otherwise the old drains, stops, and
    /// the new one loads in its place. Answers the new builder's name in `alloc` when it
    /// started now.
    pub fn rebuild<A: Allocator + Clone>(
        &mut self,
        now: Millis,
        builder: &str,
        reservation: Reservation,
        alloc: A,
    ) -> Option<Box<str, A>> {
        let e = self.executors.get(builder)?;
        if e.namespace != Namespace::Main || e.phase != ExecutorPhase::Ready {
            return None;
        }
        let machine = Box::clone_from_ref_in(e.machine.as_str(), alloc.clone());
        if let Some(id) = self.create(now, &machine, Namespace::Main, reservation.clone()) {
            let id = Box::clone_from_ref_in(id, alloc);
            let old = self.executors.get_mut(builder).expect("present");
            if old.busy.is_some() {
                old.phase = ExecutorPhase::Draining;
            } else {
                old.phase = ExecutorPhase::Stopped;
            }
            return Some(id);
        }
        self.rebuilds.insert(Box::clone_from_ref_in(builder, Global).into_string(), reservation);
        let old = self.executors.get_mut(builder).expect("present");
        if old.busy.is_some() {
            old.phase = ExecutorPhase::Draining;
        } else {
            old.phase = ExecutorPhase::Stopped;
            self.start_rebuild(now, builder);
        }
        None
    }

    fn start_rebuild(&mut self, now: Millis, old: &str) {
        // Once a rebuild chose draining, finish that lifecycle before replacing it.
        // Room freed by another executor must not restart a still-busy old builder.
        if self.executors.get(old).is_none_or(|e| e.phase != ExecutorPhase::Stopped) {
            return;
        }
        let Some(reservation) = self.rebuilds.remove(old) else { return };
        let machine = Box::clone_from_ref_in(self.executors[old].machine.as_str(), Global);
        if self.create(now, &machine, Namespace::Main, reservation.clone()).is_none() {
            // Still does not fit beside what runs there: it waits for the next stop.
            self.rebuilds.insert(Box::clone_from_ref_in(old, Global).into_string(), reservation);
        }
    }

    /// Whether every machine's reservations are within its capacity.
    pub fn check(&self) -> Result<(), CapacityExceeded<'_>> {
        let zero = Reservation { memory: 0, cpus: 0, disk: 0 };
        for (name, machine) in &self.machines {
            let used = self.used(name);
            if !fits(&zero, &used, &machine.capacity) {
                return Err(CapacityExceeded { machine: name, used, capacity: &machine.capacity });
            }
        }
        Ok(())
    }
}
