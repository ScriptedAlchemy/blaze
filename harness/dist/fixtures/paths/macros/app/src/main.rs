use derive::{Describe, Embedded, ManifestLength, Private, note};

#[derive(Describe, Embedded, Private)]
struct Widget {
    _size: u32,
}

#[derive(Describe, ManifestLength)]
struct Gadget;

#[note("a note on greet")]
fn greet() -> &'static str {
    "hello from greet"
}

fn main() {
    println!("{} {}", Widget::describe(), Gadget::describe());
    println!("{}", Widget::embedded());
    println!("{} / {}", greet(), greet_note());
    println!("manifest length {}", Gadget::manifest_length());
    println!("private {}", Widget::private());
}
