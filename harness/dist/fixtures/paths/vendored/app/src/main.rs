use std::panic;

fn main() {
    panic::set_hook(Box::new(|info| {
        let at = info.location().unwrap();
        println!("panic at {}:{}", at.file(), at.line());
    }));
    println!("random {}", fastrand::Rng::with_seed(7).u32(..));
    let caught = panic::catch_unwind(|| fastrand::Rng::with_seed(1).u8(5..=2)).is_err();
    println!("caught {caught}");
}
