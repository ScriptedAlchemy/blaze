//! Section 5's admission: which job runs, where, in which order, within which capacity.
//!
//! Each machine declares a capacity (cores and memory, less what is held back). Every executor
//! holds a [`Reservation`] on its machine from the moment it is created to the moment it
//! stops, and no machine's reservations ever sum past its capacity. Jobs wait in one queue per
//! platform: `Main` before pull requests, then first in, first out, with no job overtaking the
//! head of its class. A `Main` job runs only on a trusted builder; a pull request's only on that
//! pull request's own executor, created on its first job when it fits (section 2, rule 5). A
//! trusted builder's rebuild starts beside the old one only if both fit; otherwise the old one
//! drains first. Past `Q` waiting jobs a platform answers `busy`.

use std::collections::{BTreeMap, VecDeque};

use blaze_wire::{Digest, ExecutionId, Namespace, Reservation};

use crate::{Millis, Refusal};

/// Admission's bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// `Q`: the most executions waiting per platform.
    pub queue_limit: usize,
    /// What a `busy` answer tells the requester to wait.
    pub busy_retry_ms: u64,
    /// A pull request's executor idle this long is destroyed (section 2: 24 h).
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
    /// The class of a namespace's jobs. `Std` is read-only and runs no job; it is not one.
    pub fn of(namespace: &Namespace) -> Option<Class> {
        match namespace {
            Namespace::Main => Some(Class::Main),
            Namespace::Pr(_) => Some(Class::Pr),
            Namespace::Std => None,
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
    /// The one namespace it may ever serve: `Main` for a trusted builder, `Pr(n)` otherwise.
    pub namespace: Namespace,
    /// What it holds on its machine.
    pub reservation: Reservation,
    /// The execution it runs.
    pub busy: Option<ExecutionId>,
    /// Where it is.
    pub phase: ExecutorPhase,
    /// When it last finished a job.
    pub idle_since: Millis,
}

/// A job let in, and the executor it runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    /// The execution.
    pub execution: ExecutionId,
    /// Its namespace.
    pub namespace: Namespace,
    /// Its platform.
    pub platform: Digest,
    /// The lead executor.
    pub executor: String,
    /// Whether the executor was created for it.
    pub created: bool,
}

#[derive(Clone, Debug, Default)]
struct Queue {
    main: VecDeque<Waiting>,
    pr: VecDeque<Waiting>,
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
            machines: BTreeMap::new(),
            executors: BTreeMap::new(),
            queues: BTreeMap::new(),
            rebuilds: BTreeMap::new(),
            next_executor: 1,
        }
    }

    /// A machine and its declared capacity.
    pub fn add_machine(&mut self, name: &str, platform: Digest, capacity: Reservation) {
        self.machines.insert(name.to_owned(), Machine { platform, capacity });
    }

    /// The capacity of a machine.
    pub fn capacity(&self, machine: &str) -> Option<&Reservation> {
        self.machines.get(machine).map(|m| &m.capacity)
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
    ) -> Option<String> {
        let m = self.machines.get(machine)?;
        if !fits(&self.used(machine), &reservation, &m.capacity) {
            return None;
        }
        let id = format!("exec:{}", self.next_executor);
        self.next_executor += 1;
        self.executors.insert(
            id.clone(),
            ExecutorSlot {
                id: id.clone(),
                machine: machine.to_owned(),
                namespace,
                reservation,
                busy: None,
                phase: ExecutorPhase::Ready,
                idle_since: now,
            },
        );
        Some(id)
    }

    /// A trusted builder on `machine`, as the operator starts one: only if it fits.
    pub fn add_builder(
        &mut self,
        now: Millis,
        machine: &str,
        reservation: Reservation,
    ) -> Option<String> {
        self.create(now, machine, Namespace::Main, reservation)
    }

    /// Queues a job, or answers `busy` past `Q`.
    pub fn enqueue(&mut self, waiting: Waiting) -> Result<(), (Refusal, u64)> {
        let Some(class) = Class::of(&waiting.namespace) else {
            return Err((Refusal::Malformed("no job runs in Std".to_owned()), 0));
        };
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
    pub fn admit(&mut self, now: Millis) -> Vec<Admitted> {
        // A rebuild that waits for room tries again whenever admission runs.
        for old in self.rebuilds.keys().cloned().collect::<Vec<_>>() {
            self.start_rebuild(now, &old);
        }
        let mut admitted = Vec::new();
        let platforms: Vec<Digest> = self.queues.keys().copied().collect();
        for platform in platforms {
            loop {
                let queue = &self.queues[&platform];
                let head = match queue.main.front().or(queue.pr.front()) {
                    Some(head) => head.clone(),
                    None => break,
                };
                let Some((executor, created)) = self.place(now, &head) else { break };
                let queue = self.queues.get_mut(&platform).expect("platform");
                match Class::of(&head.namespace) {
                    Some(Class::Main) => queue.main.pop_front(),
                    _ => queue.pr.pop_front(),
                };
                self.executors.get_mut(&executor).expect("placed").busy = Some(head.execution);
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

    /// An executor for `job`: an idle one of its namespace on its platform, or, for a pull
    /// request, a new one where its reservation fits.
    fn place(&mut self, now: Millis, job: &Waiting) -> Option<(String, bool)> {
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
            return Some((e.id.clone(), false));
        }
        if job.namespace == Namespace::Main {
            // Trusted builders are started by the operator, never on demand for a job.
            return None;
        }
        let machines: Vec<String> = self
            .machines
            .iter()
            .filter(|(_, m)| m.platform == job.platform)
            .map(|(name, _)| name.clone())
            .collect();
        machines
            .iter()
            .find_map(|machine| self.create(now, machine, job.namespace.clone(), job.need.clone()))
            .map(|id| (id, true))
    }

    /// Another idle executor of `namespace` on `platform` that covers `need`, for a
    /// reassignment, not one of `except`.
    pub fn idle_for(
        &mut self,
        namespace: &Namespace,
        platform: &Digest,
        need: &Reservation,
        except: &[String],
    ) -> Option<String> {
        let id = self
            .executors
            .values()
            .find(|e| {
                e.phase == ExecutorPhase::Ready
                    && e.busy.is_none()
                    && e.namespace == *namespace
                    && covers(&e.reservation, need)
                    && !except.contains(&e.id)
                    && self.machines.get(&e.machine).is_some_and(|m| m.platform == *platform)
            })?
            .id
            .clone();
        Some(id)
    }

    /// Marks `executor` as running `execution`, for a reassignment.
    pub fn take(&mut self, executor: &str, execution: ExecutionId) {
        if let Some(e) = self.executors.get_mut(executor) {
            e.busy = Some(execution);
        }
    }

    /// The job on `executor` ended: it is idle, or, draining, stops and lets its rebuild in.
    pub fn finished(&mut self, now: Millis, executor: &str) {
        let Some(e) = self.executors.get_mut(executor) else { return };
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
    /// whose jobs end.
    pub fn close_pr(&mut self, n: u64) -> Vec<(String, Option<ExecutionId>)> {
        let mut gone = Vec::new();
        for e in self.executors.values_mut() {
            if e.namespace == Namespace::Pr(blaze_wire::PrNamespace { n })
                && e.phase != ExecutorPhase::Stopped
            {
                e.phase = ExecutorPhase::Stopped;
                gone.push((e.id.clone(), e.busy.take()));
            }
        }
        gone
    }

    /// Destroys pull-request executors idle past the bound.
    pub fn reap(&mut self, now: Millis) -> Vec<String> {
        let bound = self.config.pr_idle_ms;
        let mut gone = Vec::new();
        for e in self.executors.values_mut() {
            if matches!(e.namespace, Namespace::Pr(_))
                && e.phase == ExecutorPhase::Ready
                && e.busy.is_none()
                && now > e.idle_since + bound
            {
                e.phase = ExecutorPhase::Stopped;
                gone.push(e.id.clone());
            }
        }
        gone
    }

    /// A trusted builder's rebuild (a new identity, a new front end, growth, its job count):
    /// the new one starts beside the old only if both fit; otherwise the old drains, stops, and
    /// the new one loads in its place. Answers the new builder when it started now.
    pub fn rebuild(
        &mut self,
        now: Millis,
        builder: &str,
        reservation: Reservation,
    ) -> Option<String> {
        let e = self.executors.get(builder)?;
        if e.namespace != Namespace::Main || e.phase != ExecutorPhase::Ready {
            return None;
        }
        let machine = e.machine.clone();
        if let Some(id) = self.create(now, &machine, Namespace::Main, reservation.clone()) {
            let old = self.executors.get_mut(builder).expect("present");
            if old.busy.is_some() {
                old.phase = ExecutorPhase::Draining;
            } else {
                old.phase = ExecutorPhase::Stopped;
            }
            return Some(id);
        }
        self.rebuilds.insert(builder.to_owned(), reservation);
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
        let Some(reservation) = self.rebuilds.remove(old) else { return };
        let machine = self.executors[old].machine.clone();
        if self.create(now, &machine, Namespace::Main, reservation.clone()).is_none() {
            // Still does not fit beside what runs there: it waits for the next stop.
            self.rebuilds.insert(old.to_owned(), reservation);
        }
    }

    /// Whether every machine's reservations are within its capacity.
    pub fn check(&self) -> Result<(), String> {
        let zero = Reservation { memory: 0, cpus: 0, disk: 0 };
        for (name, machine) in &self.machines {
            let used = self.used(name);
            if !fits(&zero, &used, &machine.capacity) {
                return Err(format!(
                    "machine {name} holds {used:?}, past its capacity {:?}",
                    machine.capacity
                ));
            }
        }
        Ok(())
    }
}
