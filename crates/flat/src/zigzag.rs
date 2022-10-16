use num_bigint::ToBigInt;
use num_traits::ToPrimitive;

pub fn to_usize(x: isize) -> usize {
    let double_x = x << 1;

    if x.is_positive() || x == 0 {
        double_x as usize
    } else {
        (-double_x - 1) as usize
    }
}

pub fn to_isize(u: usize) -> isize {
    ((u >> 1) as isize) ^ (-((u & 1) as isize))
}

pub fn to_bigint(b: num_bigint::BigInt) -> num_bigint::BigInt {
    ((b.to_i128().unwrap() >> 1) ^ (-(b.to_i128().unwrap() & 1)))
        .to_bigint()
        .unwrap()
} // TODO
