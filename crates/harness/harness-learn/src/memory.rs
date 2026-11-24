//! What a person has learned, and when each piece is due again.

use crate::course::{Course, Lesson, check_key, lesson_key};
use serde::{Deserialize, Serialize};
use std::alloc::{Allocator, Global};
use std::collections::BTreeMap;

/// A day, as a count from any fixed epoch the host chooses (it only has to keep choosing the same one).
pub type Day = u32;

/// The first gap after a check is recalled, in days; each recall after multiplies it by [`GROW`].
pub const FIRST_GAP: f32 = 1.0;
/// How much a recalled check's gap grows.
pub const GROW: f32 = 2.6;
/// What is left of the gap after a miss.
pub const SHRINK: f32 = 0.35;
/// The longest gap: about four months.
pub const GAP_MAX: f32 = 120.0;
/// A check counts as learned once its gap reaches this many days (three recalls in a row from new).
pub const LEARNED_GAP: f32 = 6.0;

/// One check's memory: how long it lasts and when it is due.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// The gap, in days, the next recall is scheduled after.
    pub gap: f32,
    /// The day it is next due.
    pub due: Day,
    /// Times recalled, and times missed.
    pub recalled: u32,
    pub missed: u32,
}

impl Memory {
    /// The memory after a review on `today`: recalled first try (`right`), or missed.
    pub fn review(seen: Option<Memory>, right: bool, today: Day) -> Memory {
        let was = seen.unwrap_or(Memory { gap: 0.0, due: today, recalled: 0, missed: 0 });
        let gap = match (right, was.gap > 0.0) {
            // Recalled early (before it was due) grows the gap by the share of it that had passed, so cramming one check
            // ten times in a day does not push it out four months.
            (true, true) => {
                let waited = was.gap - (was.due.saturating_sub(today)) as f32;
                let share = (waited / was.gap).clamp(0.0, 1.0);
                (was.gap * (1.0 + (GROW - 1.0) * share)).min(GAP_MAX)
            }
            (true, false) => FIRST_GAP,
            (false, _) => was.gap * SHRINK,
        };
        // A missed check is due today again: it comes back in this run, and tomorrow's review has it.
        let due = if right { today + gap.round().max(1.0) as Day } else { today };
        Memory { gap, due, recalled: was.recalled + u32::from(right), missed: was.missed + u32::from(!right) }
    }

    pub fn learned(&self) -> bool {
        self.gap >= LEARNED_GAP
    }

    /// How well it is held, 0 to 1: the gap against the learned gap.
    pub fn strength(&self) -> f32 {
        (self.gap / LEARNED_GAP).clamp(0.0, 1.0)
    }
}

/// Days in a row with a finished run, and the last such day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streak {
    pub days: u32,
    pub last: Option<Day>,
}

impl Streak {
    /// The streak after a run finished on `today`.
    pub fn after(self, today: Day) -> Streak {
        let days = match self.last {
            Some(d) if d == today => self.days.max(1),
            Some(d) if d + 1 == today => self.days + 1,
            _ => 1,
        };
        Streak { days, last: Some(today) }
    }

    /// The streak as it stands on `today`: gone once a day was skipped.
    pub fn on(self, today: Day) -> u32 {
        match self.last {
            Some(d) if d + 1 >= today => self.days,
            _ => 0,
        }
    }
}

/// Everything remembered about one person's learning. Plain data: the host reads and writes it as JSON.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Each check's memory, by [`check_key`].
    #[serde(default)]
    pub checks: BTreeMap<String, Memory>,
    /// The lessons finished, as `course/lesson`, and the day each was first finished.
    #[serde(default)]
    pub finished: BTreeMap<String, Day>,
    #[serde(default)]
    pub streak: Streak,
}

impl Progress {
    /// Record one answer to a check on `today`. Only a check's first answer in a run is recorded (the run says which).
    pub fn record(&mut self, key: &str, right: bool, today: Day) {
        let next = Memory::review(self.checks.get(key).copied(), right, today);
        if let Some(memory) = self.checks.get_mut(key) {
            *memory = next;
        } else {
            let bytes = key.as_bytes().to_vec_in(Global);
            self.checks.insert(String::from_utf8(bytes).expect("a check key is UTF-8"), next);
        }
    }

    pub fn finish_lesson(&mut self, course: &str, lesson: &str, today: Day) {
        let mut key = Vec::new_in(Global);
        lesson_key(course, lesson, &mut key);
        self.finished.entry(String::from_utf8(key).expect("a lesson key is UTF-8")).or_insert(today);
        self.streak = self.streak.after(today);
    }

    pub fn finished<A: Allocator>(&self, course: &str, lesson: &str, alloc: A) -> bool {
        self.finished_in(course, lesson, &mut Vec::new_in(alloc))
    }

    fn finished_in<A: Allocator>(&self, course: &str, lesson: &str, key: &mut Vec<u8, A>) -> bool {
        self.finished.contains_key(lesson_key(course, lesson, key))
    }

    /// The first lesson of `course` not finished yet: where "Continue" goes. `None` once all are.
    pub fn next_lesson<'c, A: Allocator>(&self, course: &'c Course, alloc: A) -> Option<&'c Lesson> {
        let mut key = Vec::new_in(alloc);
        course.lessons.iter().find(|l| !self.finished_in(&course.id, &l.id, &mut key))
    }

    /// A lesson's mastery, 0 to 1: the mean strength of its checks (a check never answered counts as 0). A lesson whose
    /// steps are not written yet has none.
    pub fn lesson_mastery<A: Allocator>(&self, course: &str, lesson: &Lesson, alloc: A) -> f32 {
        self.lesson_mastery_in(course, lesson, &mut Vec::new_in(alloc))
    }

    fn lesson_mastery_in<A: Allocator>(&self, course: &str, lesson: &Lesson, key: &mut Vec<u8, A>) -> f32 {
        let (mut sum, mut n) = (0.0, 0);
        for id in lesson.checks() {
            sum += self.checks.get(check_key(course, &lesson.id, id, key)).map_or(0.0, Memory::strength);
            n += 1;
        }
        if n == 0 { 0.0 } else { sum / n as f32 }
    }

    /// A course's mastery, 0 to 1: the share of its lessons finished, each weighted by how well its checks are held
    /// (a finished lesson counts half on finishing and half on its mastery, so progress shows at once and then deepens).
    pub fn mastery<A: Allocator + Clone>(&self, course: &Course, alloc: A) -> f32 {
        if course.lessons.is_empty() {
            return 0.0;
        }
        let mut key = Vec::new_in(alloc);
        let total: f32 = course
            .lessons
            .iter()
            .map(|l| if self.finished_in(&course.id, &l.id, &mut key) { 0.5 + 0.5 * self.lesson_mastery_in(&course.id, l, &mut key) } else { 0.0 })
            .sum();
        total / course.lessons.len() as f32
    }

    /// The checks of `course` due on `today` or before, most overdue first: a review run's queue, as
    /// `(lesson id, check id)`. Only checks of finished lessons are due (a lesson in progress asks its own). The
    /// iterator owns its sorted queue in `alloc`; it does not allocate a second collection to discard the ranks.
    pub fn due<'c, A: Allocator + Clone>(
        &self,
        course: &'c Course,
        today: Day,
        alloc: A,
    ) -> impl ExactSizeIterator<Item = (&'c str, &'c str)> + use<'c, A> {
        let mut key = Vec::new_in(alloc.clone());
        let mut due = Vec::new_in(alloc);
        for l in &course.lessons {
            if !self.finished_in(&course.id, &l.id, &mut key) {
                continue;
            }
            for id in l.checks() {
                if let Some(m) = self.checks.get(check_key(&course.id, &l.id, id, &mut key)) {
                    if m.due <= today {
                        due.push((m.due, l.id.as_str(), id));
                    }
                }
            }
        }
        due.sort_unstable();
        due.into_iter().map(|(_, l, c)| (l, c))
    }

    /// The checks of `course` the person missed more often than they recalled, weakest first: what a tutor is told when
    /// it writes the next lesson, so the course bends toward what is shaky.
    pub fn shaky<'c, A: Allocator + Clone>(
        &self,
        course: &'c Course,
        alloc: A,
    ) -> impl ExactSizeIterator<Item = (&'c str, &'c str)> + use<'c, A> {
        let mut key = Vec::new_in(alloc.clone());
        let mut shaky = Vec::new_in(alloc);
        for l in &course.lessons {
            for id in l.checks() {
                if let Some(m) = self.checks.get(check_key(&course.id, &l.id, id, &mut key)) {
                    if m.missed > 0 && m.missed >= m.recalled {
                        shaky.push((i64::from(m.recalled) - i64::from(m.missed), l.id.as_str(), id));
                    }
                }
            }
        }
        shaky.sort_unstable();
        shaky.into_iter().map(|(_, l, c)| (l, c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::course::Step;
    use std::alloc::{AllocError, Layout};
    use std::cell::Cell;
    use std::ptr::NonNull;

    #[derive(Clone, Copy)]
    struct Count<'a> {
        calls: &'a Cell<usize>,
        live: &'a Cell<usize>,
    }

    // SAFETY: every allocation and deallocation is delegated unchanged to Global; the counters observe calls only.
    unsafe impl Allocator for Count<'_> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let memory = Global.allocate(layout)?;
            self.calls.set(self.calls.get() + 1);
            self.live.set(self.live.get() + 1);
            Ok(memory)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            self.live.set(self.live.get() - 1);
            // SAFETY: the caller supplies a live allocation from this allocator, which delegates to Global.
            unsafe { Global.deallocate(ptr, layout) };
        }
    }

    #[test]
    fn sorted_reviews_and_key_scratch_use_the_callers_allocator() {
        let lesson = |id: &str, checks: &[&str]| Lesson {
            id: id.to_owned(),
            title: id.to_owned(),
            about: String::new(),
            steps: checks.iter().map(|id| Step::Fill {
                id: (*id).to_owned(), prompt: "?".into(), answer: "a".into(), accept: Vec::new(), why: String::new(),
            }).collect(),
        };
        let course = Course { id: "k".into(), title: "K".into(), goal: String::new(), lessons: vec![lesson("z", &["b", "a", "c"]), lesson("a", &["c"])] };
        let mut p = Progress::default();
        p.finish_lesson("k", "z", 0);
        p.finish_lesson("k", "a", 0);
        for (key, due, recalled, missed) in [("k/z/b", 3, 1, 4), ("k/z/a", 1, 1, 1), ("k/z/c", 8, 1, 0), ("k/a/c", 1, 2, 2)] {
            p.checks.insert(key.to_owned(), Memory { gap: 1.0, due, recalled, missed });
        }
        let (calls, live) = (Cell::new(0), Cell::new(0));
        let alloc = Count { calls: &calls, live: &live };
        let due = p.due(&course, 3, alloc);
        assert_eq!(due.len(), 3);
        assert!(calls.get() >= 2, "both the lookup buffer and sorted queue use the caller's allocator");
        assert_eq!(live.get(), 1, "only the result queue survives the lookup");
        assert_eq!(due.collect::<Vec<_>>(), [("a", "c"), ("z", "a"), ("z", "b")]);
        assert_eq!(live.get(), 0, "consuming the result releases its allocation");

        let before = calls.get();
        let shaky = p.shaky(&course, alloc);
        assert!(calls.get() > before);
        assert_eq!(shaky.collect::<Vec<_>>(), [("z", "b"), ("a", "c"), ("z", "a")]);
        assert_eq!(live.get(), 0);

        let mut key = Vec::with_capacity_in(64, alloc);
        let address = key.as_ptr();
        let before = calls.get();
        assert_eq!(check_key("κ", "借用", "一", &mut key), "κ/借用/一");
        assert_eq!(lesson_key("κ", "借用", &mut key), "κ/借用");
        assert_eq!(key.as_ptr(), address);
        assert_eq!(calls.get(), before, "successive keys reuse the supplied capacity");
        drop(key);
        assert_eq!(live.get(), 0);
    }

    #[test]
    fn recalls_on_time_space_out_and_a_miss_pulls_back() {
        let mut m = Memory::review(None, true, 10);
        assert_eq!((m.gap, m.due), (FIRST_GAP, 11));
        m = Memory::review(Some(m), true, 11);
        assert!((m.gap - 2.6).abs() < 1e-4 && m.due == 14);
        m = Memory::review(Some(m), true, 14);
        assert!(m.learned(), "three recalls in a row from new: learned");
        let before = m.gap;
        m = Memory::review(Some(m), false, m.due);
        assert!(m.gap < before * 0.5 && !m.learned());
        assert_eq!(m.missed, 1);
    }

    #[test]
    fn cramming_a_check_the_same_day_does_not_space_it_out() {
        let mut m = Memory::review(None, true, 0);
        m = Memory::review(Some(m), true, 1);
        let gap = m.gap;
        for _ in 0..10 {
            m = Memory::review(Some(m), true, 1);
        }
        assert!((m.gap - gap).abs() < 1e-4, "recalled again the day it was scheduled: nothing had passed");
    }

    #[test]
    fn a_streak_counts_days_in_a_row_and_ends_when_one_is_skipped() {
        let s = Streak::default().after(5).after(5).after(6);
        assert_eq!(s.days, 2);
        assert_eq!((s.on(6), s.on(7), s.on(8)), (2, 2, 0));
        assert_eq!(s.after(9).days, 1);
    }
}
