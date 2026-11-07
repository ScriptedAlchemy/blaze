//! Shapes: traits and generics whose instances the units downstream compile.

use std::collections::BTreeMap;
use std::fmt::Debug;

pub trait Area {
    fn area(&self) -> f64;
    fn name(&self) -> String;
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape0(pub f64, pub u32);
impl Area for Shape0 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 0.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape0:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape1(pub f64, pub u32);
impl Area for Shape1 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 1.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape1:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape2(pub f64, pub u32);
impl Area for Shape2 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 2.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape2:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape3(pub f64, pub u32);
impl Area for Shape3 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 3.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape3:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape4(pub f64, pub u32);
impl Area for Shape4 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 4.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape4:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape5(pub f64, pub u32);
impl Area for Shape5 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 5.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape5:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape6(pub f64, pub u32);
impl Area for Shape6 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 6.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape6:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape7(pub f64, pub u32);
impl Area for Shape7 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 7.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape7:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape8(pub f64, pub u32);
impl Area for Shape8 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 8.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape8:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape9(pub f64, pub u32);
impl Area for Shape9 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 9.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape9:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape10(pub f64, pub u32);
impl Area for Shape10 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 10.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape10:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape11(pub f64, pub u32);
impl Area for Shape11 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 11.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape11:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape12(pub f64, pub u32);
impl Area for Shape12 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 12.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape12:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape13(pub f64, pub u32);
impl Area for Shape13 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 13.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape13:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape14(pub f64, pub u32);
impl Area for Shape14 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 14.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape14:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape15(pub f64, pub u32);
impl Area for Shape15 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 15.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape15:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape16(pub f64, pub u32);
impl Area for Shape16 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 16.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape16:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape17(pub f64, pub u32);
impl Area for Shape17 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 17.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape17:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape18(pub f64, pub u32);
impl Area for Shape18 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 18.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape18:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape19(pub f64, pub u32);
impl Area for Shape19 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 19.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape19:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape20(pub f64, pub u32);
impl Area for Shape20 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 20.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape20:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape21(pub f64, pub u32);
impl Area for Shape21 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 21.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape21:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape22(pub f64, pub u32);
impl Area for Shape22 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 22.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape22:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape23(pub f64, pub u32);
impl Area for Shape23 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 23.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape23:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape24(pub f64, pub u32);
impl Area for Shape24 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 24.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape24:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape25(pub f64, pub u32);
impl Area for Shape25 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 25.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape25:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape26(pub f64, pub u32);
impl Area for Shape26 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 26.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape26:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape27(pub f64, pub u32);
impl Area for Shape27 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 27.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape27:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape28(pub f64, pub u32);
impl Area for Shape28 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 28.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape28:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape29(pub f64, pub u32);
impl Area for Shape29 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 29.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape29:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape30(pub f64, pub u32);
impl Area for Shape30 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 30.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape30:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape31(pub f64, pub u32);
impl Area for Shape31 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 31.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape31:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape32(pub f64, pub u32);
impl Area for Shape32 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 32.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape32:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape33(pub f64, pub u32);
impl Area for Shape33 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 33.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape33:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape34(pub f64, pub u32);
impl Area for Shape34 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 34.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape34:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape35(pub f64, pub u32);
impl Area for Shape35 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 35.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape35:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape36(pub f64, pub u32);
impl Area for Shape36 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 36.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape36:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape37(pub f64, pub u32);
impl Area for Shape37 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 37.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape37:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape38(pub f64, pub u32);
impl Area for Shape38 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 38.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape38:{:?}", self.1)
    }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Shape39(pub f64, pub u32);
impl Area for Shape39 {
    fn area(&self) -> f64 {
        self.0 * self.0 + 39.0 + f64::from(self.1)
    }
    fn name(&self) -> String {
        format!("shape39:{:?}", self.1)
    }
}

pub fn total<T: Area + Clone + Debug>(items: &[T]) -> (f64, Vec<String>, BTreeMap<String, u64>) {
    let mut names = Vec::new();
    let mut seen = BTreeMap::new();
    let mut sum = 0.0;
    for item in items.iter().cloned() {
        sum += item.area();
        let name = format!("{}/{:?}", item.name(), item);
        *seen.entry(item.name()).or_insert(0) += 1;
        names.push(name);
    }
    names.sort();
    (sum, names, seen)
}

/// What this crate's build script wrote: a unit's input that is no source file of it.
pub const FROM_SCRIPT: &str = include_str!(concat!(env!("OUT_DIR"), "/from_script.txt"));

/// A file included from the crate: it is an input of what compiles it, as its sources are.
pub const INCLUDED: &str = include_str!("../included.txt");
