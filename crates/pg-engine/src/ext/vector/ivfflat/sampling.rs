//! PostgreSQL's row sampling (`utils/misc/sampling.c`): Knuth's Algorithm S over a table's
//! blocks and Vitter's Algorithm Z for the reservoir, with their own generators seeded from
//! pg_global_prng_state, as ANALYZE and the ivfflat build use them.

/// A pg_prng_state (xoroshiro128**), as `common/pg_prng.c` has it.
#[derive(Clone, Copy)]
pub(super) struct Prng(u64, u64);

impl Prng {
    /// `pg_prng_seed`: the state from SplitMix64 over `seed`.
    pub(super) fn seeded(mut seed: u64) -> Prng {
        let mut splitmix = || {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut v = seed;
            v = (v ^ (v >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            v = (v ^ (v >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            v ^ (v >> 31)
        };
        let (s0, s1) = (splitmix(), splitmix());
        // (pg_prng_seed_check: an all-zero state would stay zero.)
        if s0 == 0 && s1 == 0 { Prng(0x5851_F42D_4C95_7F2D, 0x1405_7B7E_F767_814F) } else { Prng(s0, s1) }
    }

    /// `xoroshiro128ss`.
    fn next(&mut self) -> u64 {
        let (s0, s1) = (self.0, self.1);
        let sx = s1 ^ s0;
        let val = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.0 = s0.rotate_left(24) ^ sx ^ (sx << 16);
        self.1 = sx.rotate_left(37);
        val
    }

    /// `pg_prng_double`: 52 random bits in [0, 1).
    fn double(&mut self) -> f64 {
        (self.next() >> 12) as f64 * (2f64).powi(-52)
    }

    /// `sampler_random_fract`: in (0, 1).
    pub(super) fn fract(&mut self) -> f64 {
        loop {
            let r = self.double();
            if r != 0.0 {
                return r;
            }
        }
    }
}

/// `BlockSamplerData`: `n` of the `N` blocks, in increasing order.
pub(super) struct BlockSampler {
    big_n: u32,
    n: i32,
    t: u32,
    m: i32,
    rand: Prng,
}

impl BlockSampler {
    /// `BlockSampler_Init`.
    pub(super) fn new(nblocks: u32, samplesize: i32, randseed: u32) -> BlockSampler {
        BlockSampler { big_n: nblocks, n: samplesize, t: 0, m: 0, rand: Prng::seeded(randseed as u64) }
    }

    /// `BlockSampler_HasMore`.
    pub(super) fn has_more(&self) -> bool {
        self.t < self.big_n && self.m < self.n
    }

    /// `BlockSampler_Next`.
    pub(super) fn next(&mut self) -> u32 {
        // remaining blocks
        let mut big_k = self.big_n - self.t;
        // blocks still to sample
        let k = self.n - self.m;
        if k as u32 >= big_k {
            // need all the rest
            self.m += 1;
            self.t += 1;
            return self.t - 1;
        }
        let v = self.rand.fract();
        let mut p = 1.0 - k as f64 / big_k as f64;
        while v < p {
            // skip
            self.t += 1;
            // keep K == N - t
            big_k -= 1;
            // adjust p to be new cutoff point in reduced range
            p *= 1.0 - k as f64 / big_k as f64;
        }
        // select
        self.m += 1;
        self.t += 1;
        self.t - 1
    }
}

/// `ReservoirStateData`: Algorithm Z's W and its generator.
pub(super) struct Reservoir {
    w: f64,
    pub(super) rand: Prng,
}

impl Reservoir {
    /// `reservoir_init_selection_state`, given the pg_prng_uint32 it draws from the global
    /// generator.
    pub(super) fn new(n: i32, seed: u32) -> Reservoir {
        let mut rand = Prng::seeded(seed as u64);
        let w = (-rand.fract().ln() / n as f64).exp();
        Reservoir { w, rand }
    }

    /// `reservoir_get_next_S`: the records to skip, `t` having been read (t >= n).
    pub(super) fn next_s(&mut self, mut t: f64, n: i32) -> f64 {
        let nf = n as f64;
        let mut s: f64;
        if t <= 22.0 * nf {
            // Process records using Algorithm X until t is large enough
            let v = self.rand.fract();
            s = 0.0;
            t += 1.0;
            // Note: "num" in Vitter's code is always equal to t - n
            let mut quot = (t - nf) / t;
            // Find min S satisfying (4.1)
            while quot > v {
                s += 1.0;
                t += 1.0;
                quot *= (t - nf) / t;
            }
        } else {
            // Now apply Algorithm Z
            let mut w = self.w;
            let term = t - nf + 1.0;
            loop {
                // Generate U and X
                let u = self.rand.fract();
                let x = t * (w - 1.0);
                // S is tentatively set to floor(X)
                s = x.floor();
                // Test if U <= h(S)/cg(X) in the manner of (6.3)
                let tmp = (t + 1.0) / term;
                let lhs = ((((u * tmp * tmp) * (term + s)) / (t + x)).ln() / nf).exp();
                let rhs = (((t + x) / (term + s)) * term) / t;
                if lhs <= rhs {
                    w = rhs / lhs;
                    break;
                }
                // Test if U <= f(S)/cg(X)
                let mut y = (((u * (t + 1.0)) / term) * (t + s + 1.0)) / (t + x);
                let (mut denom, numer_lim);
                if nf < s {
                    denom = t;
                    numer_lim = term + s;
                } else {
                    denom = t - nf + s;
                    numer_lim = t + 1.0;
                }
                let mut numer = t + s;
                while numer >= numer_lim {
                    y *= numer / denom;
                    denom -= 1.0;
                    numer -= 1.0;
                }
                // Generate W in advance
                w = (-self.rand.fract().ln() / nf).exp();
                if (y.ln() / nf).exp() <= (t + x) / t {
                    break;
                }
            }
            self.w = w;
        }
        s
    }
}
