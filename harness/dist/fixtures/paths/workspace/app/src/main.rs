use std::panic;

fn main() {
    panic::set_hook(Box::new(|info| {
        let at = info.location().unwrap();
        println!("panic at {}:{}", at.file(), at.line());
    }));
    println!("shared: {}", shared::describe());
    println!("inner: {:?} {}", inner::where_am_i(), inner::generated());
    println!("table {} blob {:?} built {}", inner::checked(5), inner::BLOB, inner::built());
    println!("doubled {} {}", shared::double(21u32), shared::double(1.5f64));
    let caught = [
        panic::catch_unwind(|| shared::boom(3)).is_err(),
        panic::catch_unwind(|| inner::out_of_range(7)).is_err(),
        panic::catch_unwind(|| fastrand::Rng::with_seed(1).u8(5..=2)).is_err(),
    ];
    println!("caught {caught:?}");
    println!("random {}", fastrand::Rng::with_seed(7).u32(..));
    let dir = inner::manifest_dir();
    println!("manifest dir ends {}", dir.rsplit('/').next().unwrap_or(dir));
}
