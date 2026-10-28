pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn triple(x: u32) -> u32 {
    x * 3
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reached only by its own code: skipped while that is the same.
    #[test]
    fn unchanged() {
        assert_eq!(double(2), 4);
    }

    #[test]
    fn other() {
        assert_eq!(triple(2), 6);
    }

    // The input the owner declares as `data`: the file in the package's directory.
    #[test]
    fn reads_data() {
        assert!(std::fs::read_to_string("data.txt").unwrap().contains("e"));
    }

    // The variable the owner declares as `env`.
    #[test]
    fn reads_env() {
        assert_ne!(std::env::var("T1_DECLARED").as_deref(), Ok("poison"));
    }

    // Pure, so a pass would be reused: the owner's `no_cache` and `external` make it run.
    #[test]
    fn opted_out() {
        assert_eq!(double(3), 6);
    }
}
