pub const fn eytzinger1_child(i: u32, child: u32) -> u32 {
    assert!(child <= 1);
    (i << 1) + child
}

pub const fn eytzinger1_left_child(i: u32) -> u32 {
    eytzinger1_child(i, 0)
}

pub const fn eytzinger1_right_child(i: u32) -> u32 {
    eytzinger1_child(i, 1)
}

const fn rounddown_pow_of_two(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        1 << (31 - v.leading_zeros())
    }
}

pub const fn eytzinger1_first(size: u32) -> u32 {
    rounddown_pow_of_two(size)
}

pub const fn eytzinger1_last(size: u32) -> u32 {
    rounddown_pow_of_two(size + 1) - 1
}

pub const fn eytzinger1_next(mut i: u32, size: u32) -> u32 {
    assert!(i != 0 && i <= size);

    if eytzinger1_right_child(i) <= size {
        i = eytzinger1_right_child(i);
        i <<= (31 - size.leading_zeros()) - (31 - i.leading_zeros());
        i >>= (i > size) as u32;
    } else {
        i >>= i.trailing_ones() + 1;
    }
    i
}

pub const fn eytzinger1_prev(mut i: u32, size: u32) -> u32 {
    assert!(i != 0 && i <= size);

    if eytzinger1_left_child(i) <= size {
        i = eytzinger1_left_child(i) + 1;
        i <<= (31 - size.leading_zeros()) - (31 - i.leading_zeros());
        i -= 1;
        i >>= (i > size) as u32;
    } else {
        i >>= i.trailing_zeros() + 1;
    }
    i
}

pub const fn eytzinger1_extra(size: u32) -> u32 {
    if size != 0 {
        (size + 1 - rounddown_pow_of_two(size)) << 1
    } else {
        0
    }
}

pub const fn __eytzinger1_to_inorder(mut i: u32, size: u32, extra: u32) -> u32 {
    assert!(i != 0 && i <= size);
    let b = 31 - i.leading_zeros();
    let shift = (31 - size.leading_zeros()) - b;

    i ^= 1 << b;
    i <<= 1;
    i |= 1;
    i <<= shift;

    if i > extra {
        i -= (i - extra) >> 1;
    }
    i
}

pub const fn __inorder_to_eytzinger1(mut i: u32, size: u32, extra: u32) -> u32 {
    assert!(i != 0 && i <= size);

    if i > extra {
        i += i - extra;
    }
    let shift = i.trailing_zeros();
    i >>= shift + 1;
    i |= 1 << ((31 - size.leading_zeros()) - shift);
    i
}

pub const fn eytzinger1_to_inorder(i: u32, size: u32) -> u32 {
    __eytzinger1_to_inorder(i, size, eytzinger1_extra(size))
}

pub const fn inorder_to_eytzinger1(i: u32, size: u32) -> u32 {
    __inorder_to_eytzinger1(i, size, eytzinger1_extra(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcachefs_eytzinger_round_trip_and_walk() {
        for size in 1..256 {
            let extra = eytzinger1_extra(size);
            for eytzinger in 1..=size {
                let inorder = __eytzinger1_to_inorder(eytzinger, size, extra);
                assert_eq!(__inorder_to_eytzinger1(inorder, size, extra), eytzinger);
            }

            let mut walked = Vec::new();
            let mut i = eytzinger1_first(size);
            while i != 0 {
                walked.push(__eytzinger1_to_inorder(i, size, extra));
                i = eytzinger1_next(i, size);
            }
            assert_eq!(walked, (1..=size).collect::<Vec<_>>());
        }
    }
}
