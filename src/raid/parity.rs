//! SIMD parity computation — AVX2/AVX-512 (x86_64) and NEON (aarch64).
//!
//! Provides XOR parity for RAID 5, the RAID 6 Q syndrome, and recovery of any
//! two lost strips of a RAID 6 stripe (`StripeStrips::recover`). Everything
//! uses the best instruction set detected at runtime (#255):
//!
//! * P and Q of a full stripe in one pass over the strips, 32 (AVX2) or 16
//!   (NEON) bytes at a time held in registers: Horner's `q = g·q ^ d`, where
//!   g·x is a shift and a conditional XOR of 0x1D per byte (as Linux's
//!   `lib/raid6/avx2.c`).
//! * Multiplying by a constant (Q's read-modify-write, recovery): two 16-entry
//!   tables, c·low-nibble and c·high-nibble, looked up with `pshufb` /
//!   `vqtbl1q` and XORed (the split-nibble method of ISA-L).
//!
//! The portable code (log tables; g·x eight lanes at a time in a u64) stays
//! for other CPUs and is what the SIMD paths are tested against.

/// Detected SIMD capability level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdLevel {
    Avx512,
    Avx2,
    Neon,
    Generic,
}

impl std::fmt::Display for SimdLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SimdLevel::Avx512 => write!(f, "AVX-512"),
            SimdLevel::Avx2 => write!(f, "AVX2"),
            SimdLevel::Neon => write!(f, "NEON"),
            SimdLevel::Generic => write!(f, "generic"),
        }
    }
}

/// Parity computation engine with runtime SIMD detection.
pub struct ParityEngine {
    pub level: SimdLevel,
}

impl ParityEngine {
    /// Detect the best SIMD level available on this CPU.
    pub fn detect() -> Self {
        let level = detect_simd();
        ParityEngine { level }
    }

    /// Create a parity engine with a specific SIMD level (for testing).
    pub fn with_level(level: SimdLevel) -> Self {
        ParityEngine { level }
    }

    /// Compute XOR parity across `data_strips` into `parity`.
    ///
    /// Used for RAID 5 P parity. `parity` is overwritten with the result.
    /// All slices must be the same length.
    pub fn compute_xor_parity(&self, data_strips: &[&[u8]], parity: &mut [u8]) {
        assert!(!data_strips.is_empty());
        let len = parity.len();
        for strip in data_strips {
            assert_eq!(strip.len(), len, "all strips must match parity buffer length");
        }

        match self.level {
            #[cfg(target_arch = "x86_64")]
            SimdLevel::Avx2 | SimdLevel::Avx512 => unsafe { xor_parity_avx2(data_strips, parity) },
            #[cfg(target_arch = "aarch64")]
            SimdLevel::Neon => unsafe { xor_parity_neon(data_strips, parity) },
            _ => xor_parity_generic(data_strips, parity),
        }
    }

    /// XOR `src` into `dst` in-place: `dst[i] ^= src[i]`.
    ///
    /// Used for partial-stripe read-modify-write parity updates.
    pub fn xor_in_place(&self, dst: &mut [u8], src: &[u8]) {
        assert_eq!(dst.len(), src.len());
        match self.level {
            #[cfg(target_arch = "x86_64")]
            SimdLevel::Avx2 | SimdLevel::Avx512 => unsafe { xor_in_place_avx2(dst, src) },
            #[cfg(target_arch = "aarch64")]
            SimdLevel::Neon => unsafe { xor_in_place_neon(dst, src) },
            _ => xor_in_place_generic(dst, src),
        }
    }

    /// Compute RAID 6 dual parity (P + Q) across `data_strips`.
    ///
    /// P = XOR of all strips (same as RAID 5).
    /// Q = GF(2^8) weighted sum: Q = g^0*D0 ^ g^1*D1 ^ ... ^ g^(n-1)*D(n-1)
    /// where g = 0x02 is the generator of GF(2^8) with polynomial 0x1D.
    pub fn compute_raid6_parity(&self, data_strips: &[&[u8]], p: &mut [u8], q: &mut [u8]) {
        assert!(!data_strips.is_empty());
        let len = p.len();
        assert_eq!(q.len(), len);
        for strip in data_strips {
            assert_eq!(strip.len(), len);
        }

        pq_at(self.level, data_strips, Some(p), q);
    }

    /// Fold a change to data strip `index` into a stripe's Q: `q ^= g^index * delta`,
    /// where `delta` is old data XOR new data — RAID-6's read-modify-write.
    pub fn q_update(&self, q: &mut [u8], delta: &[u8], index: usize) {
        gf_mul_xor_at(self.level, q, delta, gf_pow2(index));
    }

    /// Reconstruct a missing data strip from surviving strips using XOR.
    ///
    /// For RAID 5 single-disk failure: missing = XOR of all surviving + parity.
    /// `surviving` includes the parity strip.
    pub fn reconstruct_xor(&self, surviving: &[&[u8]], output: &mut [u8]) {
        self.compute_xor_parity(surviving, output);
    }
}

// --- SIMD detection ---

/// The level detected once for this process: what the free GF(2^8)
/// functions (recovery) use. A `ParityEngine` carries its own.
fn simd() -> SimdLevel {
    static LEVEL: std::sync::OnceLock<SimdLevel> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(detect_simd)
}

/// The levels this CPU can run, the portable one first (tests compare
/// every one against it).
#[cfg(test)]
fn available_levels() -> Vec<SimdLevel> {
    let mut v = vec![SimdLevel::Generic];
    let d = detect_simd();
    if d != SimdLevel::Generic {
        v.push(d);
    }
    if d == SimdLevel::Avx512 {
        v.push(SimdLevel::Avx2);
    }
    v
}

fn detect_simd() -> SimdLevel {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            return SimdLevel::Avx512;
        }
        if is_x86_feature_detected!("avx2") {
            return SimdLevel::Avx2;
        }
        return SimdLevel::Generic;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is mandatory on aarch64
        return SimdLevel::Neon;
    }
    #[allow(unreachable_code)]
    SimdLevel::Generic
}

// --- Generic (portable) implementations ---

fn xor_parity_generic(data_strips: &[&[u8]], parity: &mut [u8]) {
    let len = parity.len();
    parity.copy_from_slice(data_strips[0]);
    for strip in &data_strips[1..] {
        let mut i = 0;
        while i + 8 <= len {
            let p = u64::from_ne_bytes(parity[i..i + 8].try_into().unwrap());
            let s = u64::from_ne_bytes(strip[i..i + 8].try_into().unwrap());
            parity[i..i + 8].copy_from_slice(&(p ^ s).to_ne_bytes());
            i += 8;
        }
        while i < len {
            parity[i] ^= strip[i];
            i += 1;
        }
    }
}

fn xor_in_place_generic(dst: &mut [u8], src: &[u8]) {
    let len = dst.len();
    let mut i = 0;
    while i + 8 <= len {
        let d = u64::from_ne_bytes(dst[i..i + 8].try_into().unwrap());
        let s = u64::from_ne_bytes(src[i..i + 8].try_into().unwrap());
        dst[i..i + 8].copy_from_slice(&(d ^ s).to_ne_bytes());
        i += 8;
    }
    while i < len {
        dst[i] ^= src[i];
        i += 1;
    }
}

/// GF(2^8) multiply by 2 (the generator) with reducing polynomial 0x1D.
///
/// This is the "xtime" operation: shift left by 1, XOR with 0x1D if carry.
#[inline]
fn gf_mul2(x: u8) -> u8 {
    let carry = (x >> 7) & 1;
    (x << 1) ^ (carry * 0x1D)
}

/// Compute Q syndrome for RAID 6.
/// Q[i] = g^0 * D0[i] ^ g^1 * D1[i] ^ ... ^ g^(n-1) * D(n-1)[i]
///
/// Horner's method over whole strips, eight bytes at a time: Q = D(n-1),
/// then Q = g*Q ^ D(i) down to D0. Multiplying eight bytes by g at once is
/// the shift-and-reduce of `gf_mul2` done lane by lane in a u64.
fn compute_q_syndrome_generic(data_strips: &[&[u8]], q: &mut [u8]) {
    q.iter_mut().for_each(|b| *b = 0);
    for strip in data_strips.iter().rev() {
        q_step_generic(q, strip);
    }
}

/// P (when asked for) and Q of a stripe, with the given level.
fn pq_at(level: SimdLevel, data_strips: &[&[u8]], p: Option<&mut [u8]>, q: &mut [u8]) {
    match level {
        #[cfg(target_arch = "x86_64")]
        SimdLevel::Avx2 | SimdLevel::Avx512 => unsafe { pq_avx2(data_strips, p, q) },
        #[cfg(target_arch = "aarch64")]
        SimdLevel::Neon => unsafe { pq_neon(data_strips, p, q) },
        _ => {
            if let Some(p) = p {
                xor_parity_generic(data_strips, p);
            }
            compute_q_syndrome_generic(data_strips, q);
        }
    }
}

/// `q = g*q ^ d`, byte-wise in GF(2^8), at the process's level.
fn q_step(q: &mut [u8], d: &[u8]) {
    q_step_at(simd(), q, d)
}

fn q_step_at(level: SimdLevel, q: &mut [u8], d: &[u8]) {
    assert_eq!(q.len(), d.len());
    match level {
        #[cfg(target_arch = "x86_64")]
        SimdLevel::Avx2 | SimdLevel::Avx512 => unsafe { q_step_avx2(q, d) },
        #[cfg(target_arch = "aarch64")]
        SimdLevel::Neon => unsafe { q_step_neon(q, d) },
        _ => q_step_generic(q, d),
    }
}

fn q_step_generic(q: &mut [u8], d: &[u8]) {
    let len = q.len();
    let mut i = 0;
    while i + 8 <= len {
        let x = u64::from_ne_bytes(q[i..i + 8].try_into().unwrap());
        let s = u64::from_ne_bytes(d[i..i + 8].try_into().unwrap());
        q[i..i + 8].copy_from_slice(&(gf_mul2_x8(x) ^ s).to_ne_bytes());
        i += 8;
    }
    while i < len {
        q[i] = gf_mul2(q[i]) ^ d[i];
        i += 1;
    }
}

/// Eight independent `gf_mul2`s in one u64.
#[inline]
fn gf_mul2_x8(x: u64) -> u64 {
    let high = x & 0x8080_8080_8080_8080;
    let shifted = (x << 1) & 0xFEFE_FEFE_FEFE_FEFE;
    // Each lane whose top bit was set becomes 0x01, then 0x1D.
    shifted ^ ((high >> 7) * 0x1D)
}

// --- GF(2^8) arithmetic (polynomial 0x11D, generator 2) ---
//
// What RAID-6 recovery needs beyond multiplying by g: multiply by any
// constant, and divide. Log/antilog tables, built at compile time.

const fn gf_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11D;
        }
        i += 1;
    }
    // Doubled so a sum of two logs needs no reduction.
    let mut j = 255;
    while j < 512 {
        exp[j] = exp[j - 255];
        j += 1;
    }
    (exp, log)
}

const GF: ([u8; 512], [u8; 256]) = gf_tables();

/// a * b in GF(2^8).
#[inline]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    GF.0[GF.1[a as usize] as usize + GF.1[b as usize] as usize]
}

/// g^n.
#[inline]
pub fn gf_pow2(n: usize) -> u8 {
    GF.0[n % 255]
}

/// The multiplicative inverse of a non-zero a.
#[inline]
pub fn gf_inv(a: u8) -> u8 {
    assert!(a != 0, "zero has no inverse in GF(2^8)");
    GF.0[255 - GF.1[a as usize] as usize]
}

/// The split-nibble tables for multiplying by `c`: c·n and c·(n << 4) for
/// every nibble n. c·b = lo[b & 15] ^ hi[b >> 4], since multiplying by c is
/// linear over XOR.
fn nibble_tables(c: u8) -> ([u8; 16], [u8; 16]) {
    let (mut lo, mut hi) = ([0u8; 16], [0u8; 16]);
    for n in 0..16u8 {
        lo[n as usize] = gf_mul(c, n);
        hi[n as usize] = gf_mul(c, n << 4);
    }
    (lo, hi)
}

/// `dst ^= c * src`, byte-wise.
pub fn gf_mul_xor(dst: &mut [u8], src: &[u8], c: u8) {
    gf_mul_xor_at(simd(), dst, src, c)
}

fn gf_mul_xor_at(level: SimdLevel, dst: &mut [u8], src: &[u8], c: u8) {
    assert_eq!(dst.len(), src.len());
    if c == 0 {
        return;
    }
    match level {
        #[cfg(target_arch = "x86_64")]
        SimdLevel::Avx2 | SimdLevel::Avx512 => {
            if c == 1 {
                return unsafe { xor_in_place_avx2(dst, src) };
            }
            let t = nibble_tables(c);
            unsafe { gf_mul_avx2(dst, Some(src), &t) }
        }
        #[cfg(target_arch = "aarch64")]
        SimdLevel::Neon => {
            if c == 1 {
                return unsafe { xor_in_place_neon(dst, src) };
            }
            let t = nibble_tables(c);
            unsafe { gf_mul_neon(dst, Some(src), &t) }
        }
        _ => gf_mul_xor_generic(dst, src, c),
    }
}

fn gf_mul_xor_generic(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 1 {
        xor_in_place_generic(dst, src);
        return;
    }
    let mut table = [0u8; 256];
    for (b, t) in table.iter_mut().enumerate() {
        *t = gf_mul(c, b as u8);
    }
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= table[*s as usize];
    }
}

/// `buf = c * buf`, byte-wise.
fn gf_scale(buf: &mut [u8], c: u8) {
    gf_scale_at(simd(), buf, c)
}

fn gf_scale_at(level: SimdLevel, buf: &mut [u8], c: u8) {
    match level {
        #[cfg(target_arch = "x86_64")]
        SimdLevel::Avx2 | SimdLevel::Avx512 => unsafe { gf_mul_avx2(buf, None, &nibble_tables(c)) },
        #[cfg(target_arch = "aarch64")]
        SimdLevel::Neon => unsafe { gf_mul_neon(buf, None, &nibble_tables(c)) },
        _ => gf_scale_generic(buf, c),
    }
}

fn gf_scale_generic(buf: &mut [u8], c: u8) {
    let mut table = [0u8; 256];
    for (b, t) in table.iter_mut().enumerate() {
        *t = gf_mul(c, b as u8);
    }
    for b in buf.iter_mut() {
        *b = table[*b as usize];
    }
}

/// What a stripe could not be recovered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrecoverable {
    pub missing: usize,
    pub tolerated: usize,
}

impl std::fmt::Display for Unrecoverable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} strips of a stripe are lost; it tolerates {}", self.missing, self.tolerated)
    }
}

/// One stripe's strips, any of which may be missing, all the same length
/// when present. `q` is `None` for RAID-5 (no Q), `Some(None)` for a RAID-6
/// stripe whose Q is lost.
pub struct StripeStrips {
    pub data: Vec<Option<Vec<u8>>>,
    pub p: Option<Vec<u8>>,
    pub q: Option<Option<Vec<u8>>>,
}

impl StripeStrips {
    /// Fill in every missing strip, data and parity alike.
    ///
    /// RAID-5 recovers any one; RAID-6 any two — two data strips, a data
    /// strip with P or with Q, or P with Q (H. P. Anvin, "The mathematics of
    /// RAID-6"). Data strip `i` carries the coefficient g^i in Q.
    pub fn recover(&mut self, len: usize) -> Result<(), Unrecoverable> {
        let raid6 = self.q.is_some();
        let tolerated = if raid6 { 2 } else { 1 };
        let lost_data: Vec<usize> =
            self.data.iter().enumerate().filter(|(_, d)| d.is_none()).map(|(i, _)| i).collect();
        let p_lost = self.p.is_none();
        let q_lost = matches!(self.q, Some(None));
        let missing = lost_data.len() + p_lost as usize + q_lost as usize;
        if missing > tolerated {
            return Err(Unrecoverable { missing, tolerated });
        }

        match (lost_data.as_slice(), p_lost, q_lost) {
            ([], _, _) => {}
            ([x], false, _) => {
                // P and the other data give it back.
                let mut out = self.p.clone().unwrap();
                for (i, d) in self.data.iter().enumerate() {
                    if i != *x {
                        xor_in_place(&mut out, d.as_ref().unwrap());
                    }
                }
                self.data[*x] = Some(out);
            }
            ([x], true, false) => {
                // Q with D(x) taken as zero, then divide out g^x.
                let mut qx = vec![0u8; len];
                let zero = vec![0u8; len];
                for d in self.data.iter().rev() {
                    q_step(&mut qx, d.as_deref().unwrap_or(&zero));
                }
                let q = self.q.as_ref().unwrap().as_ref().unwrap();
                xor_in_place(&mut qx, q);
                gf_scale(&mut qx, gf_inv(gf_pow2(*x)));
                self.data[*x] = Some(qx);
            }
            ([x, y], false, false) => {
                let (x, y) = (*x, *y);
                // Pxy, Qxy: P and Q of the stripe with D(x) = D(y) = 0.
                let mut pxy = vec![0u8; len];
                let mut qxy = vec![0u8; len];
                let zero = vec![0u8; len];
                let refs: Vec<&[u8]> = self.data.iter().map(|d| d.as_deref().unwrap_or(&zero)).collect();
                pq_at(simd(), &refs, Some(&mut pxy), &mut qxy);
                let p = self.p.as_ref().unwrap();
                let q = self.q.as_ref().unwrap().as_ref().unwrap();
                xor_in_place(&mut pxy, p); // P + Pxy = Dx + Dy
                xor_in_place(&mut qxy, q); // Q + Qxy = g^x Dx + g^y Dy
                let gyx = gf_pow2(y - x);
                let denom = gf_inv(gyx ^ 1);
                let a = gf_mul(gyx, denom);
                let b = gf_mul(gf_inv(gf_pow2(x)), denom);
                let mut dx = vec![0u8; len];
                gf_mul_xor(&mut dx, &pxy, a);
                gf_mul_xor(&mut dx, &qxy, b);
                let mut dy = pxy;
                xor_in_place(&mut dy, &dx);
                self.data[x] = Some(dx);
                self.data[y] = Some(dy);
            }
            _ => unreachable!("more than the tolerated strips are lost"),
        }

        // Parity last: every data strip is present now.
        if p_lost || q_lost {
            let refs: Vec<&[u8]> = self.data.iter().map(|d| d.as_deref().unwrap()).collect();
            if p_lost {
                let mut p = vec![0u8; len];
                ParityEngine::with_level(simd()).compute_xor_parity(&refs, &mut p);
                self.p = Some(p);
            }
            if q_lost {
                let mut q = vec![0u8; len];
                pq_at(simd(), &refs, None, &mut q);
                self.q = Some(Some(q));
            }
        }
        Ok(())
    }
}

/// `dst ^= src` at the process's level.
fn xor_in_place(dst: &mut [u8], src: &[u8]) {
    ParityEngine::with_level(simd()).xor_in_place(dst, src)
}

// --- AVX2 implementations (x86_64) ---

/// g·x on 32 bytes: shift each left, XOR 0x1D into those whose top bit was
/// set (a signed compare against zero gives the mask).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mul2_avx2(x: std::arch::x86_64::__m256i) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;
    let mask = _mm256_cmpgt_epi8(_mm256_setzero_si256(), x);
    let red = _mm256_and_si256(mask, _mm256_set1_epi8(0x1D));
    _mm256_xor_si256(_mm256_add_epi8(x, x), red)
}

/// P (if asked) and Q of a stripe in one pass: each 32-byte column of
/// every strip is read once, P and Q kept in registers.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn pq_avx2(strips: &[&[u8]], mut p: Option<&mut [u8]>, q: &mut [u8]) {
    use std::arch::x86_64::*;
    let len = q.len();
    let last = strips.len() - 1;
    let mut i = 0;
    while i + 32 <= len {
        let d = _mm256_loadu_si256(strips[last].as_ptr().add(i) as *const __m256i);
        let (mut pa, mut qa) = (d, d);
        for s in strips[..last].iter().rev() {
            let d = _mm256_loadu_si256(s.as_ptr().add(i) as *const __m256i);
            pa = _mm256_xor_si256(pa, d);
            qa = _mm256_xor_si256(mul2_avx2(qa), d);
        }
        _mm256_storeu_si256(q.as_mut_ptr().add(i) as *mut __m256i, qa);
        if let Some(p) = p.as_deref_mut() {
            _mm256_storeu_si256(p.as_mut_ptr().add(i) as *mut __m256i, pa);
        }
        i += 32;
    }
    pq_tail(strips, p, q, i);
}

/// The bytes past the last whole vector, one at a time.
fn pq_tail(strips: &[&[u8]], mut p: Option<&mut [u8]>, q: &mut [u8], from: usize) {
    let last = strips.len() - 1;
    for j in from..q.len() {
        let (mut pa, mut qa) = (strips[last][j], strips[last][j]);
        for s in strips[..last].iter().rev() {
            pa ^= s[j];
            qa = gf_mul2(qa) ^ s[j];
        }
        q[j] = qa;
        if let Some(p) = p.as_deref_mut() {
            p[j] = pa;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn q_step_avx2(q: &mut [u8], d: &[u8]) {
    use std::arch::x86_64::*;
    let len = q.len();
    let mut i = 0;
    while i + 32 <= len {
        let x = _mm256_loadu_si256(q.as_ptr().add(i) as *const __m256i);
        let s = _mm256_loadu_si256(d.as_ptr().add(i) as *const __m256i);
        _mm256_storeu_si256(q.as_mut_ptr().add(i) as *mut __m256i, _mm256_xor_si256(mul2_avx2(x), s));
        i += 32;
    }
    q_step_generic(&mut q[i..], &d[i..]);
}

/// `dst ^= c·src` (with `src`), or `dst = c·dst` (without): two `pshufb`
/// lookups per 32 bytes, one per nibble.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gf_mul_avx2(dst: &mut [u8], src: Option<&[u8]>, t: &([u8; 16], [u8; 16])) {
    use std::arch::x86_64::*;
    let lo = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.0.as_ptr() as *const __m128i));
    let hi = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.1.as_ptr() as *const __m128i));
    let nib = _mm256_set1_epi8(0x0F);
    let len = dst.len();
    let mut i = 0;
    while i + 32 <= len {
        let from = match src {
            Some(s) => s.as_ptr().add(i),
            None => dst.as_ptr().add(i),
        };
        let x = _mm256_loadu_si256(from as *const __m256i);
        let l = _mm256_shuffle_epi8(lo, _mm256_and_si256(x, nib));
        let h = _mm256_shuffle_epi8(hi, _mm256_and_si256(_mm256_srli_epi16(x, 4), nib));
        let mut r = _mm256_xor_si256(l, h);
        if src.is_some() {
            r = _mm256_xor_si256(r, _mm256_loadu_si256(dst.as_ptr().add(i) as *const __m256i));
        }
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, r);
        i += 32;
    }
    gf_mul_tail(dst, src, t, i);
}

/// The bytes past the last whole vector, through the same nibble tables.
fn gf_mul_tail(dst: &mut [u8], src: Option<&[u8]>, t: &([u8; 16], [u8; 16]), from: usize) {
    for j in from..dst.len() {
        let x = match src {
            Some(s) => s[j],
            None => dst[j],
        };
        let r = t.0[(x & 15) as usize] ^ t.1[(x >> 4) as usize];
        dst[j] = if src.is_some() { dst[j] ^ r } else { r };
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn xor_parity_avx2(data_strips: &[&[u8]], parity: &mut [u8]) {
    use std::arch::x86_64::*;
    let len = parity.len();

    parity.copy_from_slice(data_strips[0]);

    for strip in &data_strips[1..] {
        let mut i = 0;
        while i + 32 <= len {
            let p = _mm256_loadu_si256(parity[i..].as_ptr() as *const __m256i);
            let s = _mm256_loadu_si256(strip[i..].as_ptr() as *const __m256i);
            let r = _mm256_xor_si256(p, s);
            _mm256_storeu_si256(parity[i..].as_mut_ptr() as *mut __m256i, r);
            i += 32;
        }
        while i < len {
            parity[i] ^= strip[i];
            i += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn xor_in_place_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::*;
    let len = dst.len();
    let mut i = 0;
    while i + 32 <= len {
        let d = _mm256_loadu_si256(dst[i..].as_ptr() as *const __m256i);
        let s = _mm256_loadu_si256(src[i..].as_ptr() as *const __m256i);
        let r = _mm256_xor_si256(d, s);
        _mm256_storeu_si256(dst[i..].as_mut_ptr() as *mut __m256i, r);
        i += 32;
    }
    while i < len {
        dst[i] ^= src[i];
        i += 1;
    }
}

// --- NEON implementations (aarch64) ---

#[cfg(target_arch = "aarch64")]
unsafe fn xor_parity_neon(data_strips: &[&[u8]], parity: &mut [u8]) {
    use std::arch::aarch64::*;
    let len = parity.len();

    parity.copy_from_slice(data_strips[0]);

    for strip in &data_strips[1..] {
        let mut i = 0;
        while i + 16 <= len {
            let p = vld1q_u8(parity[i..].as_ptr());
            let s = vld1q_u8(strip[i..].as_ptr());
            let r = veorq_u8(p, s);
            vst1q_u8(parity[i..].as_mut_ptr(), r);
            i += 16;
        }
        while i < len {
            parity[i] ^= strip[i];
            i += 1;
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn xor_in_place_neon(dst: &mut [u8], src: &[u8]) {
    use std::arch::aarch64::*;
    let len = dst.len();
    let mut i = 0;
    while i + 16 <= len {
        let d = vld1q_u8(dst[i..].as_ptr());
        let s = vld1q_u8(src[i..].as_ptr());
        let r = veorq_u8(d, s);
        vst1q_u8(dst[i..].as_mut_ptr(), r);
        i += 16;
    }
    while i < len {
        dst[i] ^= src[i];
        i += 1;
    }
}

// --- NEON GF(2^8) (aarch64) ---

/// g·x on 16 bytes: an arithmetic shift right by 7 gives the mask of bytes
/// whose top bit was set.
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn mul2_neon(x: std::arch::aarch64::uint8x16_t) -> std::arch::aarch64::uint8x16_t {
    use std::arch::aarch64::*;
    let mask = vreinterpretq_u8_s8(vshrq_n_s8::<7>(vreinterpretq_s8_u8(x)));
    veorq_u8(vshlq_n_u8::<1>(x), vandq_u8(mask, vdupq_n_u8(0x1D)))
}

#[cfg(target_arch = "aarch64")]
unsafe fn pq_neon(strips: &[&[u8]], mut p: Option<&mut [u8]>, q: &mut [u8]) {
    use std::arch::aarch64::*;
    let len = q.len();
    let last = strips.len() - 1;
    let mut i = 0;
    while i + 16 <= len {
        let d = vld1q_u8(strips[last].as_ptr().add(i));
        let (mut pa, mut qa) = (d, d);
        for s in strips[..last].iter().rev() {
            let d = vld1q_u8(s.as_ptr().add(i));
            pa = veorq_u8(pa, d);
            qa = veorq_u8(mul2_neon(qa), d);
        }
        vst1q_u8(q.as_mut_ptr().add(i), qa);
        if let Some(p) = p.as_deref_mut() {
            vst1q_u8(p.as_mut_ptr().add(i), pa);
        }
        i += 16;
    }
    pq_tail(strips, p, q, i);
}

#[cfg(target_arch = "aarch64")]
unsafe fn q_step_neon(q: &mut [u8], d: &[u8]) {
    use std::arch::aarch64::*;
    let len = q.len();
    let mut i = 0;
    while i + 16 <= len {
        let x = vld1q_u8(q.as_ptr().add(i));
        let s = vld1q_u8(d.as_ptr().add(i));
        vst1q_u8(q.as_mut_ptr().add(i), veorq_u8(mul2_neon(x), s));
        i += 16;
    }
    q_step_generic(&mut q[i..], &d[i..]);
}

#[cfg(target_arch = "aarch64")]
unsafe fn gf_mul_neon(dst: &mut [u8], src: Option<&[u8]>, t: &([u8; 16], [u8; 16])) {
    use std::arch::aarch64::*;
    let lo = vld1q_u8(t.0.as_ptr());
    let hi = vld1q_u8(t.1.as_ptr());
    let nib = vdupq_n_u8(0x0F);
    let len = dst.len();
    let mut i = 0;
    while i + 16 <= len {
        let from = match src {
            Some(s) => s.as_ptr().add(i),
            None => dst.as_ptr().add(i),
        };
        let x = vld1q_u8(from);
        let l = vqtbl1q_u8(lo, vandq_u8(x, nib));
        let h = vqtbl1q_u8(hi, vshrq_n_u8::<4>(x));
        let mut r = veorq_u8(l, h);
        if src.is_some() {
            r = veorq_u8(r, vld1q_u8(dst.as_ptr().add(i)));
        }
        vst1q_u8(dst.as_mut_ptr().add(i), r);
        i += 16;
    }
    gf_mul_tail(dst, src, t, i);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_parity_two_strips() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let a = vec![0xAA_u8; 4096];
        let b = vec![0x55_u8; 4096];
        let mut parity = vec![0u8; 4096];
        engine.compute_xor_parity(&[&a, &b], &mut parity);
        assert!(parity.iter().all(|&x| x == 0xFF));
    }

    #[test]
    fn xor_parity_three_strips() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let a = vec![0xFF_u8; 512];
        let b = vec![0x0F_u8; 512];
        let c = vec![0xF0_u8; 512];
        let mut parity = vec![0u8; 512];
        engine.compute_xor_parity(&[&a, &b, &c], &mut parity);
        assert!(parity.iter().all(|&x| x == 0x00));
    }

    #[test]
    fn xor_parity_roundtrip() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let d0 = vec![1u8; 256];
        let d1 = vec![2u8; 256];
        let d2 = vec![3u8; 256];
        let mut parity = vec![0u8; 256];
        engine.compute_xor_parity(&[&d0, &d1, &d2], &mut parity);

        // D0 ^ D1 ^ D2 ^ P should be all zeros
        let mut check = vec![0u8; 256];
        engine.compute_xor_parity(&[&d0, &d1, &d2, &parity], &mut check);
        assert!(check.iter().all(|&x| x == 0));
    }

    #[test]
    fn xor_reconstruct_missing() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let d0: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let d1: Vec<u8> = (0..256).map(|i| (i * 7) as u8).collect();
        let d2: Vec<u8> = (0..256).map(|i| (i * 13) as u8).collect();

        let mut parity = vec![0u8; 256];
        engine.compute_xor_parity(&[&d0, &d1, &d2], &mut parity);

        // Simulate d1 failure — reconstruct from d0, d2, parity
        let mut recovered = vec![0u8; 256];
        engine.reconstruct_xor(&[&d0, &d2, &parity], &mut recovered);
        assert_eq!(recovered, d1);
    }

    #[test]
    fn xor_in_place_works() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let mut dst = vec![0xAA_u8; 128];
        let src = vec![0x55_u8; 128];
        engine.xor_in_place(&mut dst, &src);
        assert!(dst.iter().all(|&x| x == 0xFF));
    }

    #[test]
    fn gf_mul2_basic() {
        assert_eq!(gf_mul2(0x00), 0x00);
        assert_eq!(gf_mul2(0x01), 0x02);
        assert_eq!(gf_mul2(0x02), 0x04);
        assert_eq!(gf_mul2(0x80), 0x1D); // overflow: carry reduces with 0x1D
    }

    #[test]
    fn raid6_pq_basic() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let d0 = vec![0x01_u8; 64];
        let d1 = vec![0x02_u8; 64];
        let d2 = vec![0x03_u8; 64];

        let mut p = vec![0u8; 64];
        let mut q = vec![0u8; 64];
        engine.compute_raid6_parity(&[&d0, &d1, &d2], &mut p, &mut q);

        // P = D0 ^ D1 ^ D2 = 0x01 ^ 0x02 ^ 0x03 = 0x00
        assert!(p.iter().all(|&x| x == 0x00));

        // Q using Horner's: start from D0, work forward
        // acc = 0; rev order: D2, D1, D0
        // step1: acc = gf_mul2(0) ^ D2 = 0x03
        // step2: acc = gf_mul2(0x03) ^ D1 = 0x06 ^ 0x02 = 0x04
        // step3: acc = gf_mul2(0x04) ^ D0 = 0x08 ^ 0x01 = 0x09
        assert!(q.iter().all(|&x| x == 0x09));
    }

    fn pattern(seed: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i as u32).wrapping_mul(2654435761).wrapping_add(seed * 97) >> 13) as u8).collect()
    }

    #[test]
    fn gf_tables_agree_with_shift_and_reduce() {
        for a in 0..=255u8 {
            assert_eq!(gf_mul(a, 2), gf_mul2(a));
            if a != 0 {
                assert_eq!(gf_mul(a, gf_inv(a)), 1);
            }
        }
        assert_eq!(gf_pow2(0), 1);
        assert_eq!(gf_pow2(8), 0x1D);
        // Byte by byte, the long way.
        let slow = |mut a: u8, mut b: u8| {
            let mut r = 0u8;
            while b != 0 {
                if b & 1 != 0 {
                    r ^= a;
                }
                a = gf_mul2(a);
                b >>= 1;
            }
            r
        };
        for a in [0u8, 1, 2, 3, 0x53, 0x80, 0xCA, 0xFF] {
            for b in [0u8, 1, 7, 0x1D, 0x8E, 0xFF] {
                assert_eq!(gf_mul(a, b), slow(a, b));
            }
        }
    }

    #[test]
    fn q_is_the_weighted_sum() {
        let strips: Vec<Vec<u8>> = (0..5).map(|s| pattern(s, 37)).collect();
        let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
        let mut q = vec![0u8; 37];
        compute_q_syndrome_generic(&refs, &mut q);
        let (mut p2, mut q2) = (vec![0u8; 37], vec![0u8; 37]);
        ParityEngine::detect().compute_raid6_parity(&refs, &mut p2, &mut q2);
        assert_eq!(q2, q, "the detected level's Q");
        for i in 0..37 {
            let mut want = 0u8;
            for (k, s) in strips.iter().enumerate() {
                want ^= gf_mul(gf_pow2(k), s[i]);
            }
            assert_eq!(q[i], want, "byte {i}");
        }
    }

    /// Every level this CPU runs gives the portable code's bytes: P and Q
    /// of stripes of 1..12 strips at lengths around the vector widths, one
    /// Q step, and multiplying by every constant (#255).
    #[test]
    fn every_simd_level_matches_the_portable_code() {
        let levels = available_levels();
        eprintln!("levels checked: {levels:?}");
        for &level in &levels {
            for n in 1..=12usize {
                for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 63, 64, 100, 4096 + 7] {
                    let strips: Vec<Vec<u8>> = (0..n as u32).map(|s| pattern(s * 7 + len as u32, len)).collect();
                    let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
                    let (mut p0, mut q0) = (vec![0u8; len], vec![0u8; len]);
                    xor_parity_generic(&refs, &mut p0);
                    compute_q_syndrome_generic(&refs, &mut q0);
                    let (mut p, mut q) = (vec![0xAAu8; len], vec![0x55u8; len]);
                    ParityEngine::with_level(level).compute_raid6_parity(&refs, &mut p, &mut q);
                    assert_eq!(p, p0, "{level} P, {n} strips, {len} bytes");
                    assert_eq!(q, q0, "{level} Q, {n} strips, {len} bytes");
                    let mut q1 = vec![0x33u8; len];
                    pq_at(level, &refs, None, &mut q1);
                    assert_eq!(q1, q0, "{level} Q alone, {n} strips, {len} bytes");
                }
            }
            for len in [0usize, 5, 16, 32, 47, 64, 1000] {
                let a = pattern(3, len);
                let d = pattern(4, len);
                let (mut want, mut got) = (a.clone(), a.clone());
                q_step_generic(&mut want, &d);
                q_step_at(level, &mut got, &d);
                assert_eq!(got, want, "{level} q_step, {len} bytes");
                for c in 0..=255u8 {
                    let (mut want, mut got) = (a.clone(), a.clone());
                    gf_mul_xor_generic(&mut want, &d, c);
                    gf_mul_xor_at(level, &mut got, &d, c);
                    assert_eq!(got, want, "{level} mul_xor by {c:#x}, {len} bytes");
                    let (mut want, mut got) = (a.clone(), a.clone());
                    gf_scale_generic(&mut want, c);
                    gf_scale_at(level, &mut got, c);
                    assert_eq!(got, want, "{level} scale by {c:#x}, {len} bytes");
                }
            }
        }
    }

    #[test]
    fn q_update_matches_a_recompute() {
        let engine = ParityEngine::with_level(SimdLevel::Generic);
        let mut strips: Vec<Vec<u8>> = (0..6).map(|s| pattern(s, 64)).collect();
        let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
        let (mut p, mut q) = (vec![0u8; 64], vec![0u8; 64]);
        engine.compute_raid6_parity(&refs, &mut p, &mut q);
        let new = pattern(99, 64);
        let mut delta = strips[4].clone();
        engine.xor_in_place(&mut delta, &new);
        engine.xor_in_place(&mut p, &delta);
        engine.q_update(&mut q, &delta, 4);
        strips[4] = new;
        let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
        let (mut p2, mut q2) = (vec![0u8; 64], vec![0u8; 64]);
        engine.compute_raid6_parity(&refs, &mut p2, &mut q2);
        assert_eq!(p, p2);
        assert_eq!(q, q2);
    }

    /// Every way of losing one or two strips of a 6+2 stripe comes back.
    #[test]
    fn raid6_recovers_any_two() {
        let len = 61;
        let n = 6;
        let data: Vec<Vec<u8>> = (0..n as u32).map(|s| pattern(s + 1, len)).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let (mut p, mut q) = (vec![0u8; len], vec![0u8; len]);
        ParityEngine::with_level(SimdLevel::Generic).compute_raid6_parity(&refs, &mut p, &mut q);
        // Strips 0..n are data, n is P, n+1 is Q.
        for a in 0..n + 2 {
            for b in a..n + 2 {
                let mut s = StripeStrips {
                    data: data.iter().cloned().map(Some).collect(),
                    p: Some(p.clone()),
                    q: Some(Some(q.clone())),
                };
                for lost in [a, b] {
                    match lost {
                        i if i < n => s.data[i] = None,
                        i if i == n => s.p = None,
                        _ => s.q = Some(None),
                    }
                }
                s.recover(len).unwrap_or_else(|e| panic!("lost {a},{b}: {e}"));
                for i in 0..n {
                    assert_eq!(s.data[i].as_ref().unwrap(), &data[i], "lost {a},{b}: data {i}");
                }
                assert_eq!(s.p.as_ref().unwrap(), &p, "lost {a},{b}: P");
                assert_eq!(s.q.as_ref().unwrap().as_ref().unwrap(), &q, "lost {a},{b}: Q");
            }
        }
    }

    #[test]
    fn raid5_recovers_one_and_refuses_two() {
        let len = 40;
        let data: Vec<Vec<u8>> = (0..4).map(|s| pattern(s, len)).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let mut p = vec![0u8; len];
        xor_parity_generic(&refs, &mut p);
        for lost in 0..5 {
            let mut s = StripeStrips { data: data.iter().cloned().map(Some).collect(), p: Some(p.clone()), q: None };
            if lost < 4 { s.data[lost] = None } else { s.p = None }
            s.recover(len).unwrap();
            assert_eq!(s.data.iter().map(|d| d.clone().unwrap()).collect::<Vec<_>>(), data);
            assert_eq!(s.p.unwrap(), p);
        }
        let mut s = StripeStrips { data: data.iter().cloned().map(Some).collect(), p: None, q: None };
        s.data[1] = None;
        assert_eq!(s.recover(len), Err(Unrecoverable { missing: 2, tolerated: 1 }));
    }

    #[test]
    fn detect_simd_runs() {
        let engine = ParityEngine::detect();
        assert!([SimdLevel::Avx512, SimdLevel::Avx2, SimdLevel::Neon, SimdLevel::Generic]
            .contains(&engine.level));
    }

    #[test]
    fn xor_parity_detected_simd() {
        let engine = ParityEngine::detect();
        let a = vec![0xAA_u8; 4096];
        let b = vec![0x55_u8; 4096];
        let mut parity = vec![0u8; 4096];
        engine.compute_xor_parity(&[&a, &b], &mut parity);
        assert!(parity.iter().all(|&x| x == 0xFF));
    }

    #[test]
    fn xor_parity_odd_length() {
        let engine = ParityEngine::detect();
        let a = vec![0xAA_u8; 100];
        let b = vec![0x55_u8; 100];
        let mut parity = vec![0u8; 100];
        engine.compute_xor_parity(&[&a, &b], &mut parity);
        assert!(parity.iter().all(|&x| x == 0xFF));
    }
}
