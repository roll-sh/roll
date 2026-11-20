//! Multi-vector values (`rank_vectors`): all of one document's vectors back to back in one
//! binary doc value, Elasticsearch's flat layout for late-interaction fields
//! (`RankVectorsFieldMapper`), with no graph and no quantization.
//!
//! Every vector of a field has the same width ([`MultiElement::vector_bytes`]), so the value
//! has no header: its vector count is its length divided by the width, and a length that isn't
//! a multiple of it is corrupt. Readers size everything from the bytes present, never from a
//! count.
//!
//! Elements: `float` as little-endian `f32`; `bfloat16` as the upper half of the float rounded
//! to nearest even, little-endian `u16` (half the bytes of `float`); `byte` as signed bytes; `bit`
//! as packed bits, eight dimensions per byte, the first in the most significant bit.

use crate::error::{Result, corrupt};

/// Element type of a multi-vector field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MultiElement {
    Float32,
    BFloat16,
    Byte,
    Bit,
}

impl MultiElement {
    /// Bytes per vector of `dims` dimensions.
    pub fn vector_bytes(self, dims: u32) -> usize {
        let d = dims as usize;
        match self {
            MultiElement::Float32 => d * 4,
            MultiElement::BFloat16 => d * 2,
            MultiElement::Byte => d,
            MultiElement::Bit => d.div_ceil(8),
        }
    }

    /// Whether values are floats (`float`, `bfloat16`) rather than bytes.
    pub fn is_float(self) -> bool {
        matches!(self, MultiElement::Float32 | MultiElement::BFloat16)
    }
}

/// A float rounded to `bfloat16` (nearest even), as its 16 bits. Java's int arithmetic: the
/// sum wraps for the bit patterns of negative NaNs.
pub fn bfloat16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    (b.wrapping_add(0x7fff).wrapping_add((b >> 16) & 1) >> 16) as u16
}

/// Appends float vectors (`values`, vector after vector) in `element`'s encoding.
/// `element` must be a float element.
pub fn encode_floats(element: MultiElement, values: &[f32], out: &mut Vec<u8>) {
    debug_assert!(element.is_float());
    match element {
        MultiElement::BFloat16 => {
            out.reserve(values.len() * 2);
            for x in values {
                out.extend_from_slice(&bfloat16_bits(*x).to_le_bytes());
            }
        }
        _ => {
            out.reserve(values.len() * 4);
            for x in values {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
}

/// One document's vectors, read in place from a doc value.
#[derive(Clone, Copy, Debug)]
pub struct MultiVectors<'a> {
    element: MultiElement,
    width: usize,
    data: &'a [u8],
}

impl<'a> MultiVectors<'a> {
    /// The vectors of `data`, a value of a field of `dims` dimensions.
    pub fn new(element: MultiElement, dims: u32, data: &'a [u8]) -> Result<MultiVectors<'a>> {
        let width = element.vector_bytes(dims);
        if width == 0 {
            return Err(corrupt("multi-vector field without dimensions"));
        }
        if !data.len().is_multiple_of(width) {
            return Err(corrupt(format!("multi-vector value of {} bytes is not a whole number of {width}-byte vectors", data.len())));
        }
        Ok(MultiVectors { element, width, data })
    }

    pub fn element(&self) -> MultiElement {
        self.element
    }

    /// Number of vectors.
    pub fn len(&self) -> usize {
        self.data.len() / self.width
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The raw bytes, vector after vector (for `byte` and `bit` elements, the values).
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    /// Elements per vector as [`MultiVectors::floats_into`] or [`MultiVectors::bytes`] give
    /// them: dimensions, or bytes for `bit` vectors.
    pub fn values_per_vector(&self) -> usize {
        match self.element {
            MultiElement::Float32 => self.width / 4,
            MultiElement::BFloat16 => self.width / 2,
            MultiElement::Byte | MultiElement::Bit => self.width,
        }
    }

    /// Appends every value as a float (`byte` and `bit` values widened from their signed bytes).
    pub fn floats_into(&self, out: &mut Vec<f32>) {
        match self.element {
            MultiElement::Float32 => {
                out.reserve(self.data.len() / 4);
                out.extend(self.data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
            }
            MultiElement::BFloat16 => {
                out.reserve(self.data.len() / 2);
                out.extend(self.data.as_chunks::<2>().0.iter().map(|c| f32::from_bits((u16::from_le_bytes(*c) as u32) << 16)));
            }
            MultiElement::Byte | MultiElement::Bit => {
                out.reserve(self.data.len());
                out.extend(self.data.iter().map(|b| *b as i8 as f32));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_each_element() {
        let mut b = Vec::new();
        encode_floats(MultiElement::Float32, &[1.5, -2.0, 3.25, 4.0, 5.0, -6.5], &mut b);
        let v = MultiVectors::new(MultiElement::Float32, 3, &b).unwrap();
        assert_eq!(v.len(), 2);
        let mut f = Vec::new();
        v.floats_into(&mut f);
        assert_eq!(f, [1.5, -2.0, 3.25, 4.0, 5.0, -6.5]);

        // bfloat16 keeps 8 bits of mantissa, rounded to nearest even, in half the bytes.
        let mut b = Vec::new();
        encode_floats(MultiElement::BFloat16, &[300.33, -0.5], &mut b);
        assert_eq!(b.len(), 4);
        let v = MultiVectors::new(MultiElement::BFloat16, 2, &b).unwrap();
        let mut f = Vec::new();
        v.floats_into(&mut f);
        assert_eq!(f, [300.0, -0.5]);

        let bytes = [8u8, 5, 0xf1, 0xff, 0x80, 2];
        let v = MultiVectors::new(MultiElement::Byte, 3, &bytes).unwrap();
        assert_eq!((v.len(), v.values_per_vector()), (2, 3));
        assert_eq!(v.bytes(), &bytes);

        let v = MultiVectors::new(MultiElement::Bit, 16, &bytes).unwrap();
        assert_eq!(v.len(), 3);
    }

    /// A value's count comes from its bytes: a length that isn't whole vectors is corrupt, and
    /// nothing is sized from anything else.
    #[test]
    fn refuses_partial_vectors() {
        assert!(MultiVectors::new(MultiElement::Float32, 3, &[0; 13]).is_err());
        assert!(MultiVectors::new(MultiElement::Bit, 0, &[0; 4]).is_err());
        assert!(MultiVectors::new(MultiElement::Byte, 4, &[]).unwrap().is_empty());
    }
}
