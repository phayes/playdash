//! Common Encryption boxes (ISO/IEC 23001-7): a sample entry's `sinf`, and a track
//! fragment's `senc`.

use super::reader::{Atom, ParseError, malformed, require};

/// A protected sample entry's `sinf`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sinf {
    /// The entry's type before protection, from `frma`.
    pub(crate) original_format: [u8; 4],
    /// `schm` scheme type, such as `cenc` or `cbcs`.
    pub(crate) scheme: [u8; 4],
    pub(crate) tenc: Tenc,
}

/// A track's default encryption parameters (`tenc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tenc {
    /// Encrypted 16-byte blocks in each pattern; `0` without a pattern (version 0).
    pub(crate) crypt_blocks: u8,
    /// Clear 16-byte blocks in each pattern.
    pub(crate) skip_blocks: u8,
    /// Whether samples are encrypted unless a sample group says otherwise.
    pub(crate) is_protected: bool,
    /// `0`, `8` or `16`; `0` when every sample uses `constant_iv`.
    pub(crate) per_sample_iv_size: u8,
    pub(crate) kid: [u8; 16],
    /// The constant IV padded with zeros, when `per_sample_iv_size` is `0` and samples
    /// are protected.
    pub(crate) constant_iv: Option<[u8; 16]>,
}

/// Parses a `sinf` box.
pub(crate) fn sinf(buf: &[u8], sinf: Atom) -> Result<Sinf, ParseError> {
    let original_format = require(sinf.children(buf), b"frma")?
        .fields(buf)
        .array::<4>()?;

    let mut schm = require(sinf.children(buf), b"schm")?.fields(buf);
    schm.version_flags()?;
    let scheme = schm.array::<4>()?;

    let schi = require(sinf.children(buf), b"schi")?;
    let mut tenc = require(schi.children(buf), b"tenc")?.fields(buf);
    let (version, _) = tenc.version_flags()?;
    tenc.skip(1)?; // reserved
    // Version 0 has a reserved byte where version 1 has the pattern.
    let pattern = tenc.u8()?;
    let (crypt_blocks, skip_blocks) = match version {
        0 => (0, 0),
        _ => (pattern >> 4, pattern & 0x0f),
    };
    let is_protected = tenc.u8()? != 0;
    let per_sample_iv_size = tenc.u8()?;
    let kid = tenc.array::<16>()?;

    let constant_iv = match (is_protected, per_sample_iv_size) {
        (false, _) | (true, 8 | 16) => None,
        (true, 0) => {
            let size = usize::from(tenc.u8()?);
            if !matches!(size, 8 | 16) {
                return Err(malformed(format!("{size}-byte constant IV")));
            }
            let mut iv = [0; 16];
            iv[..size].copy_from_slice(tenc.bytes(size)?);
            Some(iv)
        }
        (true, size) => return Err(malformed(format!("{size}-byte per-sample IV"))),
    };

    Ok(Sinf {
        original_format,
        scheme,
        tenc: Tenc {
            crypt_blocks,
            skip_blocks,
            is_protected,
            per_sample_iv_size,
            kid,
            constant_iv,
        },
    })
}

/// One sample's `senc` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SampleEncryption {
    /// The per-sample IV padded with zeros; `None` when the track uses a constant IV.
    pub(crate) iv: Option<[u8; 16]>,
    /// `(clear, protected)` byte counts; empty when the whole sample is protected.
    pub(crate) subsamples: Vec<(u16, u32)>,
}

/// Parses a `senc` box of a track whose samples carry `per_sample_iv_size`-byte IVs.
pub(crate) fn senc(
    buf: &[u8],
    senc: Atom,
    per_sample_iv_size: u8,
) -> Result<Vec<SampleEncryption>, ParseError> {
    let mut fields = senc.fields(buf);
    let (_, flags) = fields.version_flags()?;
    let count = fields.u32()?;
    let iv_size = usize::from(per_sample_iv_size);
    let mut samples = Vec::new();
    for _ in 0..count {
        let iv = match iv_size {
            0 => None,
            size => {
                let mut iv = [0; 16];
                iv[..size].copy_from_slice(fields.bytes(size)?);
                Some(iv)
            }
        };
        let mut subsamples = Vec::new();
        if flags & 0x02 != 0 {
            for _ in 0..fields.u16()? {
                subsamples.push((fields.u16()?, fields.u32()?));
            }
        }
        samples.push(SampleEncryption { iv, subsamples });
    }
    Ok(samples)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mp4::init::tests::{bx, full};
    use crate::mp4::reader::atoms;

    /// A `sinf` for `original_format` under `scheme`, with a version 1 `tenc` when
    /// `pattern` is given.
    pub(crate) fn sinf_box(
        original_format: &[u8; 4],
        scheme: &[u8; 4],
        pattern: Option<(u8, u8)>,
        iv_size: u8,
        constant_iv: Option<&[u8]>,
        kid: [u8; 16],
    ) -> Vec<u8> {
        let (version, (crypt, skip)) = match pattern {
            Some(pattern) => (1, pattern),
            None => (0, (0, 0)),
        };
        let mut tenc = vec![0, crypt << 4 | skip, 1, iv_size];
        tenc.extend(kid);
        if let Some(iv) = constant_iv {
            tenc.push(iv.len() as u8);
            tenc.extend(iv);
        }
        bx(
            b"sinf",
            &[
                &bx(b"frma", &[original_format]),
                &full(b"schm", 0, 0, &[scheme, &0x1_0000u32.to_be_bytes()]),
                &bx(b"schi", &[&full(b"tenc", version, 0, &[&tenc])]),
            ],
        )
    }

    fn parse(body: &[u8]) -> Result<Sinf, ParseError> {
        let atom = atoms(body, 0, body.len()).next().unwrap().unwrap();
        sinf(body, atom)
    }

    #[test]
    fn reads_per_sample_and_constant_ivs() {
        let parsed = parse(&sinf_box(b"mp4a", b"cenc", None, 8, None, [7; 16])).unwrap();
        assert_eq!(parsed.original_format, *b"mp4a");
        assert_eq!(parsed.scheme, *b"cenc");
        assert_eq!(
            parsed.tenc,
            Tenc {
                crypt_blocks: 0,
                skip_blocks: 0,
                is_protected: true,
                per_sample_iv_size: 8,
                kid: [7; 16],
                constant_iv: None,
            }
        );

        let parsed = parse(&sinf_box(
            b"avc1",
            b"cbcs",
            Some((1, 9)),
            0,
            Some(&[5; 8]),
            [1; 16],
        ))
        .unwrap();
        let mut iv = [0; 16];
        iv[..8].copy_from_slice(&[5; 8]);
        assert_eq!((parsed.tenc.crypt_blocks, parsed.tenc.skip_blocks), (1, 9));
        assert_eq!(parsed.tenc.constant_iv, Some(iv));
    }

    #[test]
    fn rejects_impossible_iv_sizes() {
        assert!(matches!(
            parse(&sinf_box(b"mp4a", b"cenc", None, 4, None, [0; 16])),
            Err(ParseError::Malformed(reason)) if reason == "4-byte per-sample IV"
        ));
        assert!(matches!(
            parse(&sinf_box(b"mp4a", b"cbcs", None, 0, Some(&[0; 4]), [0; 16])),
            Err(ParseError::Malformed(reason)) if reason == "4-byte constant IV"
        ));
    }
}
