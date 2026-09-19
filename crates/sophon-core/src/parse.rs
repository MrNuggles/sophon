//! Byte-level parsers for procfs/sysfs text. No allocation, no regex.

#[inline]
pub fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

pub fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if !is_space(*first) {
            break;
        }
        s = rest;
    }
    while let [rest @ .., last] = s {
        if !is_space(*last) {
            break;
        }
        s = rest;
    }
    s
}

/// Parses a leading unsigned integer (after optional whitespace); `None` if no digits.
pub fn try_u64(s: &[u8]) -> Option<u64> {
    let s = trim(s);
    let mut v: u64 = 0;
    let mut any = false;
    for &b in s {
        if !b.is_ascii_digit() {
            break;
        }
        any = true;
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u64);
    }
    any.then_some(v)
}

#[inline]
pub fn u64(s: &[u8]) -> u64 {
    try_u64(s).unwrap_or(0)
}

pub fn fields(s: &[u8]) -> impl Iterator<Item = &[u8]> {
    s.split(|&b| is_space(b)).filter(|f| !f.is_empty())
}

pub fn lines(s: &[u8]) -> impl Iterator<Item = &[u8]> {
    s.split(|&b| b == b'\n').filter(|l| !l.is_empty())
}

/// Splits `key: value` (as in `/proc/meminfo`, fdinfo), trimming both sides.
pub fn key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = line.iter().position(|&b| b == b':')?;
    Some((trim(&line[..i]), trim(&line[i + 1..])))
}

/// Parses sizes like `114844 KiB`, `2 MiB`, `0` into bytes.
pub fn size_with_unit(s: &[u8]) -> u64 {
    let n = u64(s);
    let mult = if s.ends_with(b"KiB") || s.ends_with(b"kB") {
        1024
    } else if s.ends_with(b"MiB") {
        1024 * 1024
    } else if s.ends_with(b"GiB") {
        1024 * 1024 * 1024
    } else {
        1
    };
    n * mult
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(try_u64(b"  42\n"), Some(42));
        assert_eq!(try_u64(b"x"), None);
        assert_eq!(size_with_unit(b"114844 KiB"), 114844 * 1024);
        assert_eq!(size_with_unit(b"2 MiB"), 2 * 1024 * 1024);
        assert_eq!(key_value(b"MemTotal:  10 kB"), Some((&b"MemTotal"[..], &b"10 kB"[..])));
    }
}
