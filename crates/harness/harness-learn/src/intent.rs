//! What a person typed asks for: to understand something (a lesson) or to have something done (an agent's work).
//!
//! One field takes everything; nobody picks a mode. Asking to understand ("explain bloom filters", "how does TCP
//! congestion control work", "what is a monad", a bare topic like "bloom filters") is answered by a short lesson that
//! asks back, because a person keeps what they answer and loses what they only read. Asking for work ("fix the failing
//! test", "write a script that ...") goes to an agent.

/// What a message asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent<'a> {
    /// To understand `topic`: taught as a lesson.
    Learn { topic: &'a str },
    /// Something done: an agent's work.
    Do,
}

/// The topic a learner who asks for "something" gets: the tutor picks.
pub const SURPRISE: &str = "one genuinely useful idea from computer science that most programmers never learned \
properly (your pick; name it in the course title)";

/// Openings that ask to be taught, and the words to drop after them to leave the topic.
const TEACH: [&str; 12] = [
    "teach me",
    "i want to learn",
    "i'd like to learn",
    "help me learn",
    "help me understand",
    "i want to understand",
    "explain to me",
    "explain",
    "eli5",
    "tell me about",
    "intro to",
    "introduction to",
];

/// Openings of a question about how something is: a lesson, unless the question is about the person's own work.
const QUESTION: [&str; 14] = [
    "what is",
    "what are",
    "what's",
    "whats",
    "what does",
    "how does",
    "how do",
    "how is",
    "how are",
    "why does",
    "why do",
    "why is",
    "why are",
    "difference between",
];

/// Words that make a message about work in front of the person (their code, a file, a run): an agent's, not a lesson.
const WORK: [&str; 31] = [
    "fix", "build", "make", "write", "create", "add", "run", "deploy", "install", "set up", "setup", "open", "find",
    "check", "update", "refactor", "debug", "rename", "delete", "remove", "commit", "push", "merge", "review",
    "implement", "change", "move", "test", "my ", "this ", "our ",
];

/// What `message` asks for.
#[must_use]
pub fn of(message: &str) -> Intent<'_> {
    let text = message.trim();
    // A path, a URL or code is about something in front of the person.
    if text.contains(['/', '`', '{', '\n']) || find(text, "http").is_some() {
        return teach(text).map_or(Intent::Do, |topic| Intent::Learn { topic });
    }
    if let Some(topic) = teach(text) {
        return Intent::Learn { topic };
    }
    let words = text.split_whitespace().count();
    let polite = if starts(text, "please ") { &text["please ".len()..] } else { "" };
    let starts_work = WORK.iter().any(|w| starts(text, w) || starts(polite, w));
    let about_mine = [" my ", " this ", " our ", " the repo", " the build", " the test"].iter().any(|w| find(text, w).is_some());
    if let Some(q) = QUESTION.iter().find(|q| starts(text, q)) {
        if !about_mine {
            return Intent::Learn { topic: clean(&text[q.len()..]) };
        }
        return Intent::Do;
    }
    // A bare topic: a few words with no verb of work and no question of another kind ("bloom filters").
    if words <= 5 && !starts_work && !about_mine && !text.ends_with('?') && text.chars().any(char::is_alphabetic) {
        return Intent::Learn { topic: clean(text) };
    }
    Intent::Do
}

/// The topic after an opening that asks to be taught anywhere in `text` ("hello, teach me something"), if one does.
fn teach(text: &str) -> Option<&str> {
    let (at, len) = TEACH.iter().filter_map(|k| find(text, k).map(|i| (i, k.len()))).min_by_key(|&(i, _)| i)?;
    // "explain" only opens a request at the start ("can you explain" too): mid-sentence it is a word ("this doesn't
    // explain the crash").
    let opening = text[..at].trim();
    if !opening.is_empty() && !["can you", "could you", "please", "hey", "hi", "hello", "pls"].iter().any(|p| {
        let opening = opening.trim_end_matches([',', '!']);
        opening.as_bytes().get(opening.len().saturating_sub(p.len())..).is_some_and(|end| end.eq_ignore_ascii_case(p.as_bytes()))
    }) {
        return None;
    }
    // ASCII keywords occupy the same byte positions in the original text, including after a Unicode greeting.
    let rest = &text[at + len..];
    let rest = clean(rest);
    let rest = ["about ", "how to ", "how ", "to ", "me ", "what "].iter().find_map(|p| rest.strip_prefix(p)).unwrap_or(rest).trim();
    let vague = rest.chars().filter(|c| c.is_alphanumeric()).count() < 3
        || ["something", "somthing", "anything", "a thing", "stuff"].iter().any(|v| starts(rest, v));
    Some(if vague { SURPRISE } else { rest })
}

fn starts(text: &str, word: &str) -> bool {
    text.as_bytes().get(..word.len()).is_some_and(|head| head.eq_ignore_ascii_case(word.as_bytes()))
}

fn find(text: &str, word: &str) -> Option<usize> {
    text.as_bytes().windows(word.len()).position(|part| part.eq_ignore_ascii_case(word.as_bytes()))
}

/// A topic without the punctuation and filler around it ("bloom filters work?" is "bloom filters").
fn clean(s: &str) -> &str {
    let s = s.trim().trim_start_matches([',', ':', '-']).trim().trim_end_matches(['?', '.', '!']).trim();
    [" work", " works", " actually work", " really work"].iter().rev().find_map(|w| s.strip_suffix(w)).unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learns(m: &str) -> &str {
        match of(m) {
            Intent::Learn { topic } => topic,
            Intent::Do => panic!("{m:?} was taken as work"),
        }
    }

    #[test]
    fn asking_to_understand_is_a_lesson() {
        assert_eq!(learns("explain bloom filters"), "bloom filters");
        assert_eq!(learns("Explain how bloom filters work?"), "bloom filters");
        assert_eq!(learns("how does TCP congestion control work"), "TCP congestion control");
        assert_eq!(learns("what is a monad?"), "a monad");
        assert_eq!(learns("bloom filters"), "bloom filters");
        assert_eq!(learns("can you explain raft"), "raft");
        assert_eq!(learns("teach me about lifetimes"), "lifetimes");
        assert_eq!(learns("hello teach me somthing?"), SURPRISE);
    }

    #[test]
    fn asking_for_work_is_an_agents() {
        for m in [
            "fix the failing test in crates/harness",
            "write a script that renames my photos",
            "why does my build fail",
            "what is wrong with this function",
            "deploy the site",
            "this doesn't explain the crash, look again at the logs please",
        ] {
            assert_eq!(of(m), Intent::Do, "{m:?}");
        }
    }

    #[test]
    fn a_topic_borrows_the_original_bytes_after_a_unicode_greeting() {
        // The returned topic must borrow this input's storage, independent of whether
        // the compiler merges equal string literals.
        let storage = *b"\xc4\xb0 hello, EXPLAIN ownership";
        let message = std::str::from_utf8(&storage).unwrap();
        let topic = learns(message);
        assert_eq!(topic, "ownership");
        assert_eq!(topic.as_ptr(), message[message.find("ownership").unwrap()..].as_ptr());
        // A const has no promised address identity between its uses.
        assert_eq!(learns("teach me something"), SURPRISE);
    }
}
