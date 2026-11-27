//! Prints what every instance computes, so a wrong or missing function changes the output.

fn main() {
    let n = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(7);
    let all = mathy::all(n);
    let mut hash = 0xcbf29ce484222325u64;
    for line in &all {
        for byte in line.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    println!("{} functions, {hash:016x}", all.len());
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| mathy::panics_here(n)).is_err();
    let dropped = mathy::DROPPED.load(std::sync::atomic::Ordering::SeqCst);
    println!("caught {caught}, dropped {dropped}, script says {}", shapes::FROM_SCRIPT);
    println!("included {}", shapes::INCLUDED);
    println!("{} {}", shapes::seen(), shapes::THROUGH_THE_MACRO);
}
