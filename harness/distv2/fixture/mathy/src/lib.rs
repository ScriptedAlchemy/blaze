//! Each instance here is its own function of this unit: generics over the shapes.

use shapes::*;

pub fn sum0(n: u32) -> String {
    let items: Vec<Shape0> = (0..n).map(|k| Shape0(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("0:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum1(n: u32) -> String {
    let items: Vec<Shape1> = (0..n).map(|k| Shape1(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("1:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum2(n: u32) -> String {
    let items: Vec<Shape2> = (0..n).map(|k| Shape2(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("2:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum3(n: u32) -> String {
    let items: Vec<Shape3> = (0..n).map(|k| Shape3(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("3:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum4(n: u32) -> String {
    let items: Vec<Shape4> = (0..n).map(|k| Shape4(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("4:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum5(n: u32) -> String {
    let items: Vec<Shape5> = (0..n).map(|k| Shape5(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("5:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum6(n: u32) -> String {
    let items: Vec<Shape6> = (0..n).map(|k| Shape6(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("6:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum7(n: u32) -> String {
    let items: Vec<Shape7> = (0..n).map(|k| Shape7(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("7:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum8(n: u32) -> String {
    let items: Vec<Shape8> = (0..n).map(|k| Shape8(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("8:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum9(n: u32) -> String {
    let items: Vec<Shape9> = (0..n).map(|k| Shape9(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("9:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum10(n: u32) -> String {
    let items: Vec<Shape10> = (0..n).map(|k| Shape10(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("10:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum11(n: u32) -> String {
    let items: Vec<Shape11> = (0..n).map(|k| Shape11(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("11:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum12(n: u32) -> String {
    let items: Vec<Shape12> = (0..n).map(|k| Shape12(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("12:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum13(n: u32) -> String {
    let items: Vec<Shape13> = (0..n).map(|k| Shape13(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("13:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum14(n: u32) -> String {
    let items: Vec<Shape14> = (0..n).map(|k| Shape14(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("14:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum15(n: u32) -> String {
    let items: Vec<Shape15> = (0..n).map(|k| Shape15(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("15:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum16(n: u32) -> String {
    let items: Vec<Shape16> = (0..n).map(|k| Shape16(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("16:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum17(n: u32) -> String {
    let items: Vec<Shape17> = (0..n).map(|k| Shape17(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("17:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum18(n: u32) -> String {
    let items: Vec<Shape18> = (0..n).map(|k| Shape18(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("18:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum19(n: u32) -> String {
    let items: Vec<Shape19> = (0..n).map(|k| Shape19(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("19:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum20(n: u32) -> String {
    let items: Vec<Shape20> = (0..n).map(|k| Shape20(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("20:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum21(n: u32) -> String {
    let items: Vec<Shape21> = (0..n).map(|k| Shape21(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("21:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum22(n: u32) -> String {
    let items: Vec<Shape22> = (0..n).map(|k| Shape22(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("22:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum23(n: u32) -> String {
    let items: Vec<Shape23> = (0..n).map(|k| Shape23(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("23:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum24(n: u32) -> String {
    let items: Vec<Shape24> = (0..n).map(|k| Shape24(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("24:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum25(n: u32) -> String {
    let items: Vec<Shape25> = (0..n).map(|k| Shape25(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("25:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum26(n: u32) -> String {
    let items: Vec<Shape26> = (0..n).map(|k| Shape26(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("26:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum27(n: u32) -> String {
    let items: Vec<Shape27> = (0..n).map(|k| Shape27(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("27:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum28(n: u32) -> String {
    let items: Vec<Shape28> = (0..n).map(|k| Shape28(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("28:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum29(n: u32) -> String {
    let items: Vec<Shape29> = (0..n).map(|k| Shape29(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("29:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum30(n: u32) -> String {
    let items: Vec<Shape30> = (0..n).map(|k| Shape30(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("30:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum31(n: u32) -> String {
    let items: Vec<Shape31> = (0..n).map(|k| Shape31(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("31:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum32(n: u32) -> String {
    let items: Vec<Shape32> = (0..n).map(|k| Shape32(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("32:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum33(n: u32) -> String {
    let items: Vec<Shape33> = (0..n).map(|k| Shape33(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("33:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum34(n: u32) -> String {
    let items: Vec<Shape34> = (0..n).map(|k| Shape34(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("34:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum35(n: u32) -> String {
    let items: Vec<Shape35> = (0..n).map(|k| Shape35(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("35:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum36(n: u32) -> String {
    let items: Vec<Shape36> = (0..n).map(|k| Shape36(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("36:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum37(n: u32) -> String {
    let items: Vec<Shape37> = (0..n).map(|k| Shape37(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("37:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum38(n: u32) -> String {
    let items: Vec<Shape38> = (0..n).map(|k| Shape38(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("38:{sum}:{}:{}", names.len(), seen.len())
}

pub fn sum39(n: u32) -> String {
    let items: Vec<Shape39> = (0..n).map(|k| Shape39(f64::from(k), k)).collect();
    let (sum, names, seen) = total(&items);
    format!("39:{sum}:{}:{}", names.len(), seen.len())
}

pub fn all(n: u32) -> Vec<String> {
    vec![
        sum0(n),
        sum1(n),
        sum2(n),
        sum3(n),
        sum4(n),
        sum5(n),
        sum6(n),
        sum7(n),
        sum8(n),
        sum9(n),
        sum10(n),
        sum11(n),
        sum12(n),
        sum13(n),
        sum14(n),
        sum15(n),
        sum16(n),
        sum17(n),
        sum18(n),
        sum19(n),
        sum20(n),
        sum21(n),
        sum22(n),
        sum23(n),
        sum24(n),
        sum25(n),
        sum26(n),
        sum27(n),
        sum28(n),
        sum29(n),
        sum30(n),
        sum31(n),
        sum32(n),
        sum33(n),
        sum34(n),
        sum35(n),
        sum36(n),
        sum37(n),
        sum38(n),
        sum39(n),
    ]
}

/// Panics in this crate, caught in the caller's: its cleanup and landing pads are this unit's
/// compiled code, wherever an executor compiled it.
pub fn panics_here(n: u32) -> u32 {
    let _noted = Noted(n);
    if n % 2 == 1 {
        panic!("odd: {n}");
    }
    n
}

struct Noted(u32);

impl Drop for Noted {
    fn drop(&mut self) {
        DROPPED.fetch_add(self.0, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What every [`panics_here`] dropped, panic or no panic.
pub static DROPPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
