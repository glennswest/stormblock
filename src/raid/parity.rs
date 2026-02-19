//! SIMD parity computation — AVX2/AVX-512 (x86_64) and NEON (aarch64).
//!
//! Provides XOR parity for RAID 5 and GF(2^8) multiply for RAID 6 Q syndrome.
//! Runtime detection selects the best available instruction set.

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

        // P is just XOR parity
        self.compute_xor_parity(data_strips, p);

        // Q = GF(2^8) weighted sum
        compute_q_syndrome_generic(data_strips, q);
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
fn compute_q_syndrome_generic(data_strips: &[&[u8]], q: &mut [u8]) {
    let len = q.len();
    q.iter_mut().for_each(|b| *b = 0);

    // Use Horner's method: Q = Dn-1 ^ g*(Dn-2 ^ g*(... ^ g*D0))
    for i in 0..len {
        let mut acc: u8 = 0;
        for strip in data_strips.iter().rev() {
            acc = gf_mul2(acc) ^ strip[i];
        }
        q[i] = acc;
    }
}

// --- AVX2 implementations (x86_64) ---

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
