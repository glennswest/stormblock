//! Parity arithmetic for parity-protected volumes, over whole slab slots.
//!
//! A stripe is `data_width` consecutive virtual extents plus one (P) or two
//! (P and Q) parity slots. P is the XOR of the members; Q is the GF(2^8)
//! weighted sum `Σ g^i · D_i` with generator `g = 2` over the polynomial
//! `0x1D` — the same field `raid/parity.rs` computes Q in, so a stripe here
//! and a stripe there mean the same thing. An unallocated member is all
//! zeros, which is what makes allocate-on-write and parity agree: adding a
//! member to a stripe is a delta from zero.
//!
//! Erasure coding (#159): with three or four parity legs (`ec:K+M`, M 3–4,
//! K+M ≤ 24) the legs are Reed–Solomon rows of a Cauchy matrix,
//! `c(j, i) = 1 / (x_j ⊕ y_i)` with `x_j = j` and `y_i = M + i`. Every square
//! submatrix of a Cauchy matrix is invertible, so any M shards lost (members
//! or legs, mixed) are recovered. P and P+Q (one or two legs, RAID-5/6) keep
//! their coefficients and their code (#159 decision 2a): the parity count says
//! which code a stripe is in, and EC never has fewer than three.
//!
//! Everything here is pure: buffers in, buffers out, no I/O, no locks. The
//! volume owns the ordering.

use std::fmt;

/// GF(2^8) multiply with reducing polynomial 0x1D (the RAID-6 field).
pub fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let carry = a & 0x80;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    p
}

/// `g^i` for the generator 2.
pub fn gf_pow2(i: usize) -> u8 {
    let mut v = 1u8;
    for _ in 0..(i % 255) {
        v = gf_mul(v, 2);
    }
    v
}

/// Multiplicative inverse; `gf_inv(0)` is undefined and returns 0.
pub fn gf_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    // a^(254) = a^-1 in GF(2^8).
    let mut result = 1u8;
    let mut base = a;
    let mut e = 254u32;
    while e > 0 {
        if e & 1 == 1 {
            result = gf_mul(result, base);
        }
        base = gf_mul(base, base);
        e >>= 1;
    }
    result
}

/// `dst[i] ^= src[i]`.
pub fn xor_into(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= *s;
    }
}

/// `dst[i] ^= coef · src[i]` in GF(2^8): the RAID engine's split-nibble
/// SIMD multiply (#255), the same field.
pub fn mul_xor_into(dst: &mut [u8], src: &[u8], coef: u8) {
    debug_assert_eq!(dst.len(), src.len());
    if coef == 1 {
        return xor_into(dst, src);
    }
    crate::raid::parity::gf_mul_xor(dst, src, coef);
}

/// The most parity legs a stripe has (#159: `ec:K+4`).
pub const MAX_PARITY: u8 = 4;
/// The widest erasure-coded stripe, members and legs (#159).
pub const MAX_EC_WIDTH: usize = 24;

/// Leg `leg`'s coefficient for member `member` in a stripe of `parity` legs:
/// P is 1, Q is `g^i` (RAID-5/6); with three or more legs, the Cauchy row.
pub fn coef(parity: u8, leg: usize, member: usize) -> u8 {
    if parity <= 2 {
        if leg == 0 { 1 } else { gf_pow2(member) }
    } else {
        let x = leg as u8;
        let y = parity.wrapping_add(member as u8);
        gf_inv(x ^ y)
    }
}

/// Compute the parity legs for a stripe. `members[i]` is member `i`'s full
/// slot, or `None` for one that is not allocated (zeros). Returns `parity`
/// buffers: P, then Q if `parity == 2`; the Cauchy rows with three or more.
pub fn compute_parity(members: &[Option<&[u8]>], slot_len: usize, parity: u8) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..parity.max(1)).map(|_| vec![0u8; slot_len]).collect();
    for (i, m) in members.iter().enumerate() {
        if let Some(d) = m {
            debug_assert_eq!(d.len(), slot_len);
            for (j, leg) in out.iter_mut().enumerate() {
                mul_xor_into(leg, d, coef(parity.max(1), j, i));
            }
        }
    }
    out
}

/// Fold a change to member `member` into one parity leg's range: `delta =
/// old ^ new` over the bytes that changed.
pub fn apply_delta_leg(leg_buf: &mut [u8], parity: u8, leg: usize, member: usize, delta: &[u8]) {
    mul_xor_into(leg_buf, delta, coef(parity, leg, member));
}

/// Fold a change to member `i` into the parity ranges: `delta = old ^ new`
/// over the bytes that changed. `p` and `q` are the same byte range of the
/// parity slots.
pub fn apply_delta(p: &mut [u8], q: Option<&mut [u8]>, member_idx: usize, delta: &[u8]) {
    xor_into(p, delta);
    if let Some(q) = q {
        mul_xor_into(q, delta, gf_pow2(member_idx));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeError {
    /// More members are missing than the surviving parity can recover.
    Unrecoverable { missing: usize, parity_available: usize },
}

impl fmt::Display for StripeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StripeError::Unrecoverable { missing, parity_available } => write!(
                f,
                "{missing} member(s) missing with {parity_available} parity leg(s) readable"
            ),
        }
    }
}

impl std::error::Error for StripeError {}

/// Rebuild the missing members of a stripe in place from whichever of its
/// `parity` legs could be read (`legs[j]`, `None` for one that could not):
/// P and P+Q as [`reconstruct`] does, the Cauchy rows by solving for up to
/// as many members as legs survive.
pub fn reconstruct_legs(
    members: &mut [Option<Vec<u8>>],
    legs: &[Option<&[u8]>],
    parity: u8,
    slot_len: usize,
) -> Result<(), StripeError> {
    if parity <= 2 {
        return reconstruct(members, legs.first().copied().flatten(), legs.get(1).copied().flatten(), slot_len);
    }
    let missing: Vec<usize> = members.iter().enumerate().filter(|(_, m)| m.is_none()).map(|(i, _)| i).collect();
    let rows: Vec<usize> = legs.iter().enumerate().filter(|(_, l)| l.is_some()).map(|(j, _)| j).collect();
    if missing.is_empty() {
        return Ok(());
    }
    if missing.len() > rows.len() {
        return Err(StripeError::Unrecoverable { missing: missing.len(), parity_available: rows.len() });
    }
    let n = missing.len();
    let rows = &rows[..n];
    // What the missing members must sum to, row by row: the leg with every
    // present member's share taken out.
    let rhs: Vec<Vec<u8>> = rows
        .iter()
        .map(|&j| {
            let mut acc = legs[j].unwrap().to_vec();
            for (i, m) in members.iter().enumerate() {
                if let Some(d) = m {
                    mul_xor_into(&mut acc, d, coef(parity, j, i));
                }
            }
            acc
        })
        .collect();
    // The n×n system A·X = rhs, A[r][c] = coef(row r, missing c); invert A
    // by Gauss–Jordan (a Cauchy submatrix is never singular).
    let mut a: Vec<Vec<u8>> = rows.iter().map(|&j| missing.iter().map(|&x| coef(parity, j, x)).collect()).collect();
    let mut inv: Vec<Vec<u8>> = (0..n).map(|r| (0..n).map(|c| (r == c) as u8).collect()).collect();
    for col in 0..n {
        let pivot = (col..n).find(|&r| a[r][col] != 0).ok_or(StripeError::Unrecoverable {
            missing: n,
            parity_available: rows.len(),
        })?;
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let k = gf_inv(a[col][col]);
        for c in 0..n {
            a[col][c] = gf_mul(a[col][c], k);
            inv[col][c] = gf_mul(inv[col][c], k);
        }
        for r in 0..n {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..n {
                    let (ac, ic) = (gf_mul(f, a[col][c]), gf_mul(f, inv[col][c]));
                    a[r][c] ^= ac;
                    inv[r][c] ^= ic;
                }
            }
        }
    }
    // X[c] = Σ_r inv[c][r] · rhs[r].
    let solved: Vec<Vec<u8>> = (0..n)
        .map(|c| {
            let mut x = vec![0u8; slot_len];
            for (r, b) in rhs.iter().enumerate() {
                mul_xor_into(&mut x, b, inv[c][r]);
            }
            x
        })
        .collect();
    for (c, x) in missing.iter().zip(solved) {
        members[*c] = Some(x);
    }
    Ok(())
}

/// Rebuild the missing members of a stripe in place.
///
/// `members[i]` is `None` for a member that could not be read (an
/// unallocated member is `Some(zeros)`, not `None`). `p` and `q` are the
/// parity slots that *could* be read. Handles: one missing with P; one
/// missing with only Q; two missing with P and Q.
pub fn reconstruct(
    members: &mut [Option<Vec<u8>>],
    p: Option<&[u8]>,
    q: Option<&[u8]>,
    slot_len: usize,
) -> Result<(), StripeError> {
    let missing: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, m)| m.is_none())
        .map(|(i, _)| i)
        .collect();
    let parity_available = p.is_some() as usize + q.is_some() as usize;
    match missing.len() {
        0 => Ok(()),
        1 => {
            let x = missing[0];
            if let Some(p) = p {
                // D_x = P ^ XOR(others)
                let mut d = p.to_vec();
                for (i, m) in members.iter().enumerate() {
                    if i != x {
                        xor_into(&mut d, m.as_ref().unwrap());
                    }
                }
                members[x] = Some(d);
                Ok(())
            } else if let Some(q) = q {
                // D_x = inv(g^x) · (Q ^ Σ_{j≠x} g^j D_j)
                let mut acc = q.to_vec();
                for (j, m) in members.iter().enumerate() {
                    if j != x {
                        mul_xor_into(&mut acc, m.as_ref().unwrap(), gf_pow2(j));
                    }
                }
                let inv = gf_inv(gf_pow2(x));
                for b in acc.iter_mut() {
                    *b = gf_mul(inv, *b);
                }
                members[x] = Some(acc);
                Ok(())
            } else {
                Err(StripeError::Unrecoverable { missing: 1, parity_available })
            }
        }
        2 => {
            let (Some(p), Some(q)) = (p, q) else {
                return Err(StripeError::Unrecoverable { missing: 2, parity_available });
            };
            let (x, y) = (missing[0], missing[1]);
            // Pxy = P ^ XOR(others) = D_x ^ D_y
            // Qxy = Q ^ Σ_{j∉{x,y}} g^j D_j = g^x D_x ^ g^y D_y
            let mut pxy = p.to_vec();
            let mut qxy = q.to_vec();
            for (j, m) in members.iter().enumerate() {
                if j != x && j != y {
                    let d = m.as_ref().unwrap();
                    xor_into(&mut pxy, d);
                    mul_xor_into(&mut qxy, d, gf_pow2(j));
                }
            }
            // g^y Pxy ^ Qxy = (g^x ^ g^y) D_x
            let gx = gf_pow2(x);
            let gy = gf_pow2(y);
            let denom_inv = gf_inv(gx ^ gy);
            let mut dx = vec![0u8; slot_len];
            for i in 0..slot_len {
                let num = gf_mul(gy, pxy[i]) ^ qxy[i];
                dx[i] = gf_mul(denom_inv, num);
            }
            let mut dy = pxy;
            xor_into(&mut dy, &dx);
            members[x] = Some(dx);
            members[y] = Some(dy);
            Ok(())
        }
        n => Err(StripeError::Unrecoverable { missing: n, parity_available }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn field_basics() {
        assert_eq!(gf_mul(1, 0x53), 0x53);
        assert_eq!(gf_pow2(0), 1);
        assert_eq!(gf_pow2(1), 2);
        assert_eq!(gf_pow2(8), 0x1D);
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "inverse of {a}");
        }
    }

    #[test]
    fn q_matches_the_raid_engine() {
        // The drive-level engine's Q must equal ours — one field, one meaning.
        let (d0, d1, d2) = (pattern(1, 64), pattern(7, 64), pattern(9, 64));
        let engine = crate::raid::parity::ParityEngine::with_level(
            crate::raid::parity::SimdLevel::Generic,
        );
        let mut p = vec![0u8; 64];
        let mut q = vec![0u8; 64];
        engine.compute_raid6_parity(&[&d0, &d1, &d2], &mut p, &mut q);
        let ours = compute_parity(&[Some(&d0), Some(&d1), Some(&d2)], 64, 2);
        assert_eq!(ours[0], p);
        assert_eq!(ours[1], q);
    }

    #[test]
    fn delta_update_equals_recompute() {
        let len = 128;
        let (d0, mut d1, d2) = (pattern(3, len), pattern(5, len), pattern(8, len));
        let mut par = compute_parity(&[Some(&d0), Some(&d1), Some(&d2)], len, 2);
        let new1 = pattern(77, len);
        let mut delta = d1.clone();
        xor_into(&mut delta, &new1);
        // Update a sub-range only.
        let (a, b) = (16, 80);
        let (p, q) = par.split_at_mut(1);
        apply_delta(&mut p[0][a..b], Some(&mut q[0][a..b]), 1, &delta[a..b]);
        d1[a..b].copy_from_slice(&new1[a..b]);
        let fresh = compute_parity(&[Some(&d0), Some(&d1), Some(&d2)], len, 2);
        assert_eq!(par, fresh);
    }

    #[test]
    fn unallocated_members_are_zero() {
        let len = 32;
        let d1 = pattern(4, len);
        let par = compute_parity(&[None, Some(&d1), None], len, 1);
        assert_eq!(par[0], d1);
    }

    #[test]
    fn reconstruct_one_with_p() {
        let len = 96;
        let (d0, d1, d2) = (pattern(1, len), pattern(2, len), pattern(3, len));
        let par = compute_parity(&[Some(&d0), Some(&d1), Some(&d2)], len, 1);
        let mut m = vec![Some(d0.clone()), None, Some(d2.clone())];
        reconstruct(&mut m, Some(&par[0]), None, len).unwrap();
        assert_eq!(m[1].as_ref().unwrap(), &d1);
    }

    #[test]
    fn reconstruct_one_with_only_q() {
        let len = 96;
        let (d0, d1, d2) = (pattern(1, len), pattern(2, len), pattern(3, len));
        let par = compute_parity(&[Some(&d0), Some(&d1), Some(&d2)], len, 2);
        for x in 0..3 {
            let mut m = vec![Some(d0.clone()), Some(d1.clone()), Some(d2.clone())];
            m[x] = None;
            reconstruct(&mut m, None, Some(&par[1]), len).unwrap();
            let want = [&d0, &d1, &d2][x];
            assert_eq!(m[x].as_ref().unwrap(), want, "member {x} from Q");
        }
    }

    #[test]
    fn reconstruct_two_with_p_and_q() {
        let len = 96;
        let ds: Vec<Vec<u8>> = (0..5).map(|i| pattern(10 + i, len)).collect();
        let refs: Vec<Option<&[u8]>> = ds.iter().map(|d| Some(d.as_slice())).collect();
        let par = compute_parity(&refs, len, 2);
        for x in 0..5 {
            for y in (x + 1)..5 {
                let mut m: Vec<Option<Vec<u8>>> = ds.iter().cloned().map(Some).collect();
                m[x] = None;
                m[y] = None;
                reconstruct(&mut m, Some(&par[0]), Some(&par[1]), len).unwrap();
                assert_eq!(m[x].as_ref().unwrap(), &ds[x], "member {x} of ({x},{y})");
                assert_eq!(m[y].as_ref().unwrap(), &ds[y], "member {y} of ({x},{y})");
            }
        }
    }

    /// #159: RAID-5/6's coefficients are what they were.
    #[test]
    fn p_and_q_keep_their_coefficients() {
        for i in 0..8 {
            assert_eq!(coef(1, 0, i), 1);
            assert_eq!(coef(2, 0, i), 1);
            assert_eq!(coef(2, 1, i), gf_pow2(i));
        }
    }

    /// Every way of losing up to M of the K+M shards (members and legs,
    /// mixed) recovers the members, for the widths the decision names.
    fn every_erasure_pattern_recovers(k: usize, m: u8) {
        let len = 48;
        let ds: Vec<Vec<u8>> = (0..k).map(|i| pattern(40 + i as u8, len)).collect();
        let refs: Vec<Option<&[u8]>> = ds.iter().map(|d| Some(d.as_slice())).collect();
        let legs = compute_parity(&refs, len, m);
        assert_eq!(legs.len(), m as usize);
        let n = k + m as usize;
        let mut patterns = 0;
        for mask in 0u32..(1u32 << n) {
            if mask.count_ones() as usize > m as usize {
                continue;
            }
            let mut members: Vec<Option<Vec<u8>>> =
                (0..k).map(|i| if mask & (1 << i) != 0 { None } else { Some(ds[i].clone()) }).collect();
            let lrefs: Vec<Option<&[u8]>> = (0..m as usize)
                .map(|j| if mask & (1 << (k + j)) != 0 { None } else { Some(legs[j].as_slice()) })
                .collect();
            reconstruct_legs(&mut members, &lrefs, m, len).unwrap_or_else(|e| panic!("{k}+{m} mask {mask:b}: {e}"));
            for i in 0..k {
                assert_eq!(members[i].as_ref().unwrap(), &ds[i], "{k}+{m} mask {mask:b} member {i}");
            }
            patterns += 1;
        }
        assert!(patterns > 1);
    }

    #[test]
    fn ec_8_3_recovers_any_three() {
        every_erasure_pattern_recovers(8, 3);
    }

    #[test]
    fn ec_16_4_recovers_any_four() {
        every_erasure_pattern_recovers(16, 4);
    }

    #[test]
    fn ec_narrow_and_wide_recover() {
        every_erasure_pattern_recovers(2, 3);
        every_erasure_pattern_recovers(20, 4);
    }

    #[test]
    fn ec_one_more_than_m_is_an_error() {
        let len = 16;
        let ds: Vec<Vec<u8>> = (0..4).map(|i| pattern(i, len)).collect();
        let refs: Vec<Option<&[u8]>> = ds.iter().map(|d| Some(d.as_slice())).collect();
        let legs = compute_parity(&refs, len, 3);
        let mut m: Vec<Option<Vec<u8>>> = vec![None, None, Some(ds[2].clone()), Some(ds[3].clone())];
        let l: Vec<Option<&[u8]>> = vec![Some(&legs[0]), None, None];
        assert_eq!(
            reconstruct_legs(&mut m, &l, 3, len),
            Err(StripeError::Unrecoverable { missing: 2, parity_available: 1 })
        );
    }

    #[test]
    fn ec_delta_update_equals_recompute() {
        for m in [3u8, 4] {
            let len = 64;
            let mut ds: Vec<Vec<u8>> = (0..6).map(|i| pattern(60 + i, len)).collect();
            let refs: Vec<Option<&[u8]>> = ds.iter().map(|d| Some(d.as_slice())).collect();
            let mut legs = compute_parity(&refs, len, m);
            let new3 = pattern(99, len);
            let mut delta = ds[3].clone();
            xor_into(&mut delta, &new3);
            let (a, b) = (8, 40);
            for (j, leg) in legs.iter_mut().enumerate() {
                apply_delta_leg(&mut leg[a..b], m, j, 3, &delta[a..b]);
            }
            ds[3][a..b].copy_from_slice(&new3[a..b]);
            let refs: Vec<Option<&[u8]>> = ds.iter().map(|d| Some(d.as_slice())).collect();
            assert_eq!(legs, compute_parity(&refs, len, m), "ec with {m} legs");
        }
    }

    #[test]
    fn too_many_missing_is_an_error() {
        let len = 16;
        let d = pattern(1, len);
        let par = compute_parity(&[Some(&d), Some(&d), Some(&d)], len, 1);
        let mut m = vec![None, None, Some(d.clone())];
        assert_eq!(
            reconstruct(&mut m, Some(&par[0]), None, len),
            Err(StripeError::Unrecoverable { missing: 2, parity_available: 1 })
        );
        let mut m = vec![None, Some(d.clone()), Some(d.clone())];
        assert!(reconstruct(&mut m, None, None, len).is_err());
    }
}
