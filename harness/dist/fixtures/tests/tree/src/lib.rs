#[cfg(test)]
mod tests {
    #[test]
    fn nested_file() {
        assert_eq!(std::fs::read_to_string("d/a/b").unwrap(), "hello\n");
    }
}
