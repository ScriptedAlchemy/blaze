//! What a tutor (an agent) is asked, as plain text: a course's outline, then each lesson's steps when it is opened.
//!
//! The rules in [`steps`] are the design of the lessons: they are what makes a course here active (asked, not told),
//! short, and bent toward what this person keeps missing.

use crate::course::{Course, Lesson, OPTIONS_MAX, READ_WORDS_MAX, STEPS_MAX, Step};
use std::io::{self, Write};

/// Ask for a course on `topic` with its first lesson written: its title, goal and three to six lessons, the first with
/// its steps, so the learner starts answering after one wait; the rest are written when opened ([`steps`]).
pub fn outline(topic: &str, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
    write!(
        out,
        "You are a tutor. A learner asked about:\n\n{topic}\n\n\
Plan a short, active course for them: 3 to 6 lessons that build on each other, each teaching ONE idea that fits in \
about three minutes. The first lesson must stand alone: someone who stops after it has understood the heart of what \
they asked. Start where a newcomer to exactly this topic has to start, and end with them able to do something real.\n\n\
Write the FIRST lesson's steps now; the others get only a title and their one idea.\n\n"
    )?;
    rules(out)?;
    write!(
        out,
        "\n\nAnswer with JSON only, no other words:\n\
{{\"id\": \"kebab-case-id\", \"title\": \"at most 5 words\", \"goal\": \"one line: what they can do at the end\", \
\"lessons\": [{{\"id\": \"kebab-case\", \"title\": \"at most 5 words\", \"about\": \"one line: the one idea\", \
\"steps\": {shape}}}, {{\"id\": \"...\", \"title\": \"...\", \"about\": \"...\"}}]}}\n\
Every check's `id` is short and unique in the lesson. Do not use tools; answer from what you know.",
        shape = SHAPE,
    )
}

/// The shape of a lesson's steps in the JSON asked for: one of each kind.
const SHAPE: &str = "[\n\
  {\"kind\": \"read\", \"title\": \"...\", \"body\": \"...\", \"code\": \"\"},\n\
  {\"kind\": \"choice\", \"id\": \"c1\", \"prompt\": \"...\", \"code\": \"\", \"options\": [\"...\"], \"answer\": 0, \"why\": \"...\"},\n\
  {\"kind\": \"fill\", \"id\": \"f1\", \"prompt\": \"... ___ ...\", \"answer\": \"...\", \"accept\": [], \"why\": \"...\"},\n\
  {\"kind\": \"order\", \"id\": \"o1\", \"prompt\": \"...\", \"items\": [\"...\"], \"why\": \"...\"}\n\
]";

/// How a lesson is written: the design of every lesson, in the tutor's words. People learn by answering, not by
/// reading.
fn rules(out: &mut (impl Write + ?Sized)) -> io::Result<()> {
    write!(
        out,
        "Write a lesson as steps. People learn by answering, not by reading, so follow every rule:\n\
1. 8 to 12 steps (never more than {STEPS_MAX}). Most are checks. Never two `read` steps in a row.\n\
2. A `read` tells ONE small idea in at most 60 words (hard limit {READ_WORDS_MAX}), plain and concrete, with one tiny \
example. Put code in `code`, not in `body`.\n\
3. Every idea a `read` tells is asked back by the very next step, in a new example, so it takes thinking and not \
matching words.\n\
4. Checks get harder through the lesson: recall it, then apply it, then predict what happens or what breaks. The last \
two checks mix ideas from the whole lesson.\n\
5. `choice`: 3 or 4 options (at most {OPTIONS_MAX}). The wrong options are the mistakes a learner really makes. \
`answer` is the 0-based index of the right one; vary where it is.\n\
6. `fill`: the answer is one word or one short token; list other spellings that should count in `accept`.\n\
7. `order`: 3 to 5 `items`, given in the correct order (they are shown shuffled).\n\
8. `why`: one or two sentences: why the right answer is right, and why the tempting wrong one is wrong.\n\
9. No trick questions, no \"all of the above\", no questions about wording."
    )
}

/// One earlier check the learner missed, as the tutor is told it.
fn missed_line<'a>(course: &'a Course, lesson: &str, check: &str) -> Option<&'a str> {
    let step = course.lesson(lesson)?.steps.iter().find(|s| s.check_id() == Some(check))?;
    let prompt = match step {
        Step::Choice { prompt, .. } | Step::Fill { prompt, .. } | Step::Order { prompt, .. } => prompt,
        Step::Read { .. } => return None,
    };
    Some(prompt)
}

/// Ask for `lesson`'s steps. `shaky` are earlier checks of `course` the learner keeps missing
/// ([`crate::Progress::shaky`]): the lesson asks each again in a new way, so the course bends toward them.
pub fn steps<'a>(
    course: &Course,
    lesson: &Lesson,
    shaky: impl IntoIterator<Item = (&'a str, &'a str)>,
    out: &mut (impl Write + ?Sized),
) -> io::Result<()> {
    let at = course.lessons.iter().position(|l| l.id == lesson.id).map_or(0, |i| i + 1);
    write!(
        out,
        "You are a tutor writing one lesson of the course \"{title}\" (goal: {goal}).\n\n\
Lesson {at} of {n}: \"{lt}\". Its one idea: {about}\n\n\
Lessons the learner already finished:\n",
        title = course.title,
        goal = course.goal,
        n = course.lessons.len(),
        lt = lesson.title,
        about = lesson.about,
    )?;
    for l in course.lessons.iter().take(at.saturating_sub(1)) {
        writeln!(out, "- {}: {}", l.title, l.about)?;
    }
    if at <= 1 {
        writeln!(out, "(none: this is the first lesson)")?;
    }
    let mut missed = shaky.into_iter().take(4).filter_map(|(l, c)| missed_line(course, l, c)).peekable();
    if missed.peek().is_some() {
        write!(
            out,
            "\nThe learner missed these earlier questions. Ask each idea again here in a NEW example (never the same \
wording), early in the lesson:\n"
        )?;
        for prompt in missed {
            writeln!(out, "- {prompt}")?;
        }
    }
    writeln!(out)?;
    rules(out)?;
    write!(
        out,
        "\n\nAnswer with JSON only, no other words:\n\
{{\"steps\": {shape}}}\n\
Every check's `id` is short and unique in the lesson. Do not use tools; answer from what you know.",
        shape = SHAPE,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lessons_prompt_names_what_came_before_and_what_was_missed() {
        let first = Lesson {
            id: "a".into(),
            title: "Owning".into(),
            about: "One owner".into(),
            steps: vec![Step::Fill { id: "f".into(), prompt: "A value has one ___".into(), answer: "owner".into(), accept: Vec::new(), why: "w".into() }],
        };
        let second = Lesson { id: "b".into(), title: "Borrowing".into(), about: "References".into(), steps: Vec::new() };
        let c = Course { id: "c".into(), title: "Ownership".into(), goal: "Read errors".into(), lessons: vec![first, second.clone()] };
        let mut bytes = Vec::new();
        steps(&c, &second, [("a", "f")], &mut bytes).unwrap();
        let p = std::str::from_utf8(&bytes).unwrap();
        assert!(p.contains("Lesson 2 of 2: \"Borrowing\""));
        assert!(p.contains("- Owning: One owner"));
        assert!(p.contains("- A value has one ___"));
        bytes.clear();
        outline("rust lifetimes", &mut bytes).unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("rust lifetimes"));
    }

    #[test]
    fn a_prompt_writes_into_fixed_storage_and_reports_a_full_sink() {
        let mut storage = [0u8; 8192];
        let capacity = storage.len();
        let mut sink = &mut storage[..];
        outline("借用", &mut sink as &mut dyn Write).unwrap();
        let written = capacity - sink.len();
        let prompt = std::str::from_utf8(&storage[..written]).unwrap();
        assert!(prompt.starts_with("You are a tutor. A learner asked about:\n\n借用\n\n"));
        assert!(prompt.ends_with("Do not use tools; answer from what you know."));

        let mut tiny = [0u8; 8];
        let error = outline("借用", &mut &mut tiny[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    }
}
