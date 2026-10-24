//! Multiprecision integers (`pgp-mpi.c`) and the RSA / Elgamal arithmetic on them
//! (`pgp-mpi-openssl.c`).

use super::super::digest::Md;
use super::super::px::{PXE_PGP_MATH_FAILED, Px, PxErr};
use super::mbuf::{Reader, Writer};
use super::{Pgp, debug, random};
use num_bigint::BigUint;

/// `PGP_MPI`: its bit count as the packet says, and (bits + 7) / 8 bytes.
#[derive(Clone, Debug, Default)]
pub(super) struct Mpi {
    pub bits: usize,
    pub data: Vec<u8>,
}

impl Mpi {
    /// `pgp_mpi_create`: the first (bits + 7) / 8 bytes of `data`.
    pub fn create(data: &[u8], bits: usize) -> Mpi {
        Mpi {
            bits,
            data: data[..bits.div_ceil(8)].to_vec(),
        }
    }

    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    /// `pgp_mpi_read`.
    pub fn read(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<Mpi> {
        let hdr = r.read_fixed(level, 2, ctx)?;
        let bits = ((hdr[0] as usize) << 8) + hdr[1] as usize;
        let data = r.read_fixed(level, bits.div_ceil(8), ctx)?;
        Ok(Mpi { bits, data })
    }

    /// `pgp_mpi_write`.
    pub fn write(&self, w: &mut Writer, level: usize) -> Px<()> {
        w.write(level, &[(self.bits >> 8) as u8, self.bits as u8])?;
        w.write(level, &self.data)
    }

    /// `pgp_mpi_hash`.
    pub fn hash(&self, md: &mut Md) {
        md.update(&[(self.bits >> 8) as u8, self.bits as u8]);
        md.update(&self.data);
    }

    /// `pgp_mpi_cksum`.
    pub fn cksum(&self, cksum: u32) -> u32 {
        let mut c = cksum + (self.bits as u32 >> 8) + (self.bits as u32 & 0xFF);
        for &b in &self.data {
            c += b as u32;
        }
        c & 0xFFFF
    }

    /// `mpi_to_bn`: None (after a debug message) when the bit count is wrong.
    fn to_bn(&self) -> Option<BigUint> {
        let bn = BigUint::from_bytes_be(&self.data);
        if bn.bits() as usize != self.bits {
            debug(format!("mpi_to_bn: bignum conversion failed: mpi={}, bn={}", self.bits, bn.bits()));
            return None;
        }
        Some(bn)
    }

    /// `bn_to_mpi`.
    fn from_bn(bn: &BigUint) -> Mpi {
        let bits = bn.bits() as usize;
        let data = if bits == 0 { Vec::new() } else { bn.to_bytes_be() };
        Mpi { bits, data }
    }
}

fn bns(ms: &[&Mpi]) -> Px<Vec<BigUint>> {
    // Every conversion runs (each may log), then any failure counts.
    let v: Vec<Option<BigUint>> = ms.iter().map(|m| m.to_bn()).collect();
    v.into_iter().collect::<Option<Vec<_>>>().ok_or(PxErr::Code(PXE_PGP_MATH_FAILED))
}

/// `decide_k_bits`.
fn k_bits(p_bits: usize) -> usize {
    if p_bits <= 5120 { p_bits / 10 + 160 } else { (p_bits / 8 + 200) * 3 / 2 }
}

/// `BN_rand(k, bits, 0, 0)`: `bits` random bits, the top one set.
fn rand_bits(bits: usize) -> BigUint {
    let mut b = vec![0u8; bits.div_ceil(8)];
    random(&mut b);
    let mut k = BigUint::from_bytes_be(&b);
    k &= (BigUint::from(1u8) << bits) - 1u8;
    k |= BigUint::from(1u8) << (bits - 1);
    k
}

/// `pgp_elgamal_encrypt`: (g^k, m * y^k) mod p.
pub(super) fn elgamal_encrypt(p: &Mpi, g: &Mpi, y: &Mpi, m: &Mpi) -> Px<(Mpi, Mpi)> {
    let v = bns(&[m, p, g, y])?;
    let (m, p, g, y) = (&v[0], &v[1], &v[2], &v[3]);
    if p.bits() == 0 {
        return Err(PxErr::Code(PXE_PGP_MATH_FAILED));
    }
    let k = rand_bits(k_bits(p.bits() as usize));
    let c1 = g.modpow(&k, p);
    let c2 = (m * y.modpow(&k, p)) % p;
    Ok((Mpi::from_bn(&c1), Mpi::from_bn(&c2)))
}

/// `pgp_elgamal_decrypt`: c2 / c1^x mod p.
pub(super) fn elgamal_decrypt(p: &Mpi, x: &Mpi, c1: &Mpi, c2: &Mpi) -> Px<Mpi> {
    let v = bns(&[c1, c2, p, x])?;
    let (c1, c2, p, x) = (&v[0], &v[1], &v[2], &v[3]);
    if p.bits() == 0 {
        return Err(PxErr::Code(PXE_PGP_MATH_FAILED));
    }
    let div = c1.modpow(x, p).modinv(p).ok_or(PxErr::Code(PXE_PGP_MATH_FAILED))?;
    Ok(Mpi::from_bn(&((c2 * div) % p)))
}

/// `pgp_rsa_encrypt`: m^e mod n.
pub(super) fn rsa_encrypt(n: &Mpi, e: &Mpi, m: &Mpi) -> Px<Mpi> {
    let v = bns(&[m, e, n])?;
    if v[2].bits() == 0 {
        return Err(PxErr::Code(PXE_PGP_MATH_FAILED));
    }
    Ok(Mpi::from_bn(&v[0].modpow(&v[1], &v[2])))
}

/// `pgp_rsa_decrypt`: c^d mod n.
pub(super) fn rsa_decrypt(n: &Mpi, d: &Mpi, c: &Mpi) -> Px<Mpi> {
    let v = bns(&[c, d, n])?;
    if v[2].bits() == 0 {
        return Err(PxErr::Code(PXE_PGP_MATH_FAILED));
    }
    Ok(Mpi::from_bn(&v[0].modpow(&v[1], &v[2])))
}
