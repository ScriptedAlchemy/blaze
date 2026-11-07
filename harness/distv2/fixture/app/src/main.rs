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
}
