#[cfg(test)]
mod tests {
    // Fails on the first attempt the file `attempts.txt` has seen and passes on the next, the
    // way a test that fails now and then does. The file is not one the owner declares.
    #[test]
    fn flaky_once() {
        let seen = std::fs::read_to_string("attempts.txt")
            .ok()
            .and_then(|it| it.trim().parse::<u32>().ok())
            .unwrap_or(0);
        std::fs::write("attempts.txt", (seen + 1).to_string()).unwrap();
        assert!(seen >= 1, "attempt {}", seen + 1);
    }

    #[test]
    fn steady() {
        assert_eq!(1 + 1, 2);
    }
}
