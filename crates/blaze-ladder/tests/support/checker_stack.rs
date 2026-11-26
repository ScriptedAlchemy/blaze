/// The recursive front end uses the builder's stack budget. Preserve the original test panic.
pub fn on_checker_stack<R: Send + 'static>(run: impl FnOnce() -> R + Send + 'static) -> R {
    match std::thread::Builder::new().stack_size(256 << 20).spawn(run).expect("fixture thread").join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
