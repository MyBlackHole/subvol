pub const JHASH_INITVAL: u32 = 0xdead_beef;

pub const fn jhash_size(n: u32) -> u32 {
    1u32 << n
}

pub const fn jhash_mask(n: u32) -> u32 {
    jhash_size(n) - 1
}

fn __jhash_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

fn __jhash_final(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

pub unsafe fn jhash(key: *const core::ffi::c_void, mut length: u32, initval: u32) -> u32 {
    let mut k = key.cast::<u8>();
    let mut a = JHASH_INITVAL.wrapping_add(length).wrapping_add(initval);
    let mut b = a;
    let mut c = a;

    while length > 12 {
        a = a.wrapping_add(u32::from_ne_bytes(core::ptr::read_unaligned(k.cast())));
        b = b.wrapping_add(u32::from_ne_bytes(core::ptr::read_unaligned(
            k.add(4).cast(),
        )));
        c = c.wrapping_add(u32::from_ne_bytes(core::ptr::read_unaligned(
            k.add(8).cast(),
        )));
        __jhash_mix(&mut a, &mut b, &mut c);
        length -= 12;
        k = k.add(12);
    }

    if length >= 12 {
        c = c.wrapping_add((*k.add(11) as u32) << 24);
    }
    if length >= 11 {
        c = c.wrapping_add((*k.add(10) as u32) << 16);
    }
    if length >= 10 {
        c = c.wrapping_add((*k.add(9) as u32) << 8);
    }
    if length >= 9 {
        c = c.wrapping_add(*k.add(8) as u32);
    }
    if length >= 8 {
        b = b.wrapping_add((*k.add(7) as u32) << 24);
    }
    if length >= 7 {
        b = b.wrapping_add((*k.add(6) as u32) << 16);
    }
    if length >= 6 {
        b = b.wrapping_add((*k.add(5) as u32) << 8);
    }
    if length >= 5 {
        b = b.wrapping_add(*k.add(4) as u32);
    }
    if length >= 4 {
        a = a.wrapping_add((*k.add(3) as u32) << 24);
    }
    if length >= 3 {
        a = a.wrapping_add((*k.add(2) as u32) << 16);
    }
    if length >= 2 {
        a = a.wrapping_add((*k.add(1) as u32) << 8);
    }
    if length >= 1 {
        a = a.wrapping_add(*k as u32);
        __jhash_final(&mut a, &mut b, &mut c);
    }
    c
}

pub unsafe fn jhash2(k: *const u32, mut length: u32, initval: u32) -> u32 {
    let mut k = k;
    let mut a = JHASH_INITVAL
        .wrapping_add(length << 2)
        .wrapping_add(initval);
    let mut b = a;
    let mut c = a;

    while length > 3 {
        a = a.wrapping_add(*k);
        b = b.wrapping_add(*k.add(1));
        c = c.wrapping_add(*k.add(2));
        __jhash_mix(&mut a, &mut b, &mut c);
        length -= 3;
        k = k.add(3);
    }

    if length >= 3 {
        c = c.wrapping_add(*k.add(2));
    }
    if length >= 2 {
        b = b.wrapping_add(*k.add(1));
    }
    if length >= 1 {
        a = a.wrapping_add(*k);
        __jhash_final(&mut a, &mut b, &mut c);
    }
    c
}

pub fn __jhash_nwords(mut a: u32, mut b: u32, mut c: u32, initval: u32) -> u32 {
    a = a.wrapping_add(initval);
    b = b.wrapping_add(initval);
    c = c.wrapping_add(initval);
    __jhash_final(&mut a, &mut b, &mut c);
    c
}

pub fn jhash_3words(a: u32, b: u32, c: u32, initval: u32) -> u32 {
    __jhash_nwords(
        a,
        b,
        c,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(3 << 2),
    )
}

pub fn jhash_2words(a: u32, b: u32, initval: u32) -> u32 {
    __jhash_nwords(
        a,
        b,
        0,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(2 << 2),
    )
}

pub fn jhash_1word(a: u32, initval: u32) -> u32 {
    __jhash_nwords(
        a,
        0,
        0,
        initval.wrapping_add(JHASH_INITVAL).wrapping_add(1 << 2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_local_linux_jhash_vectors() {
        unsafe {
            assert_eq!(jhash_size(8), 256);
            assert_eq!(jhash_mask(8), 255);
            let mut bytes = [0u8; 20];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = i as u8 * 7 + 3;
            }
            let words = [
                0x1020_3040,
                0x2040_6080,
                0x3060_90c0,
                0x4080_c100,
                0x50a0_f140,
            ];

            assert_eq!(jhash(bytes.as_ptr().cast(), 0, 0), 0xdead_beef);
            assert_eq!(
                jhash(bytes.as_ptr().cast(), bytes.len() as u32, 0x1357_9bdf),
                0x4120_6d53
            );
            assert_eq!(
                jhash2(words.as_ptr(), words.len() as u32, 0x2468_ace0),
                0x9d46_9ecc
            );
            assert_eq!(jhash_1word(0x1122_3344, 7), 0x42e2_6ba7);
            assert_eq!(jhash_2words(0x1122_3344, 0x5566_7788, 7), 0xc1f0_0f29);
            assert_eq!(
                jhash_3words(0x1122_3344, 0x5566_7788, 0x99aa_bbcc, 7),
                0x388c_8834
            );
        }
    }
}
