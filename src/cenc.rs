//! ISO/IEC 23001-7 Common Encryption for fragmented MP4 audio.
//!
//! Decrypts `cenc` (AES-128-CTR) and `cbcs` (AES-128-CBC with a crypt/skip
//! pattern) samples in place. Decryption never changes a body's length, so
//! segment sizes from ranges and HEAD stay valid. The protection boxes
//! (`sinf`, `senc`, `saiz`, `saio`, `pssh`) stay where they are; only each
//! protected sample entry's type is restored from `enca` to its `frma`
//! original, so Symphonia recognizes the codec.

// Decryption runs where segments are fetched, so without `sync` or `async`
// only `ContentKeys` is reachable.
#![cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]

use crate::error::Error;
use crate::mp4::init::TrackDefaults;
use crate::mp4::reader::{ParseError, atoms, find, fourcc};
use crate::mp4::{self, fragment, protection};
use crate::segment::SegmentLocation;
use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit, KeyIvInit, StreamCipher};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
#[cfg(any(feature = "sync", feature = "async"))]
use std::sync::Arc;

/// `cenc` keystream: the IV's high 8 bytes are fixed and the low 8 bytes
/// count blocks.
type Aes128Ctr64 = ctr::Ctr64BE<Aes128>;

const BLOCK: usize = 16;

/// Content keys for decrypting Common Encryption, by 128-bit key ID (KID).
///
/// `Debug` lists key IDs but never keys.
///
/// # Examples
///
/// ```
/// use playdash::ContentKeys;
///
/// let mut keys = ContentKeys::new();
/// keys.insert_hex(
///     "01234567-89ab-cdef-0123-456789abcdef",
///     "00112233445566778899aabbccddeeff",
/// )?;
///
/// assert_eq!(keys.len(), 1);
/// # Ok::<(), playdash::Error>(())
/// ```
#[derive(Clone, Default)]
pub struct ContentKeys {
    keys: HashMap<[u8; 16], [u8; 16]>,
}

impl ContentKeys {
    /// Creates an empty key set.
    ///
    /// Add keys with [`Self::insert`] when they are already decoded into 16-byte
    /// arrays, or [`Self::insert_hex`] when they come from a manifest or command line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Associates the 128-bit content key `key` with the 128-bit key ID `kid`.
    ///
    /// A later insertion with the same key ID replaces the earlier key. The returned
    /// mutable reference makes several insertions easy to chain.
    pub fn insert(&mut self, kid: [u8; 16], key: [u8; 16]) -> &mut Self {
        self.keys.insert(kid, key);
        self
    }

    /// Parses and adds a hexadecimal key ID and content key.
    ///
    /// Each value must contain exactly 32 hexadecimal digits. Dashes are ignored, so a
    /// UUID-form `cenc:default_KID` can be passed directly. The key ID and key follow the
    /// same order as mp4decrypt's `--key kid:key` option.
    ///
    /// Returns [`Error::InvalidContentKey`] without including the supplied key text in
    /// the error message when either value is malformed.
    pub fn insert_hex(&mut self, kid: &str, key: &str) -> Result<&mut Self, Error> {
        let kid = parse_hex_128(kid)?;
        let key = parse_hex_128(key)?;
        Ok(self.insert(kid, key))
    }

    /// Returns whether this set contains a content key for `kid`.
    pub fn contains(&self, kid: &[u8; 16]) -> bool {
        self.keys.contains_key(kid)
    }

    /// Returns the number of distinct key IDs in the set.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Returns `true` when the set contains no content keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Adds every key from `other` without replacing an existing key.
    ///
    /// Returns a conflicting key ID when both sets contain different keys for that ID.
    /// No keys are added when a conflict is found.
    pub(crate) fn merge(&mut self, other: ContentKeys) -> Result<(), [u8; 16]> {
        let conflict = other
            .keys
            .iter()
            .find(|&(kid, key)| self.keys.get(kid).is_some_and(|known| known != key));
        if let Some((kid, _)) = conflict {
            return Err(*kid);
        }
        self.keys.extend(other.keys);
        Ok(())
    }

    fn key(&self, kid: &[u8; 16]) -> Result<&[u8; 16], Problem> {
        self.keys.get(kid).ok_or(Problem::MissingKey(*kid))
    }
}

impl fmt::Debug for ContentKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kids: Vec<String> = self.keys.keys().map(|kid| hex(kid)).collect();
        f.debug_struct("ContentKeys")
            .field("kids", &kids)
            .finish_non_exhaustive()
    }
}

fn parse_hex_128(value: &str) -> Result<[u8; 16], Error> {
    let digits: Vec<u8> = value.trim().bytes().filter(|&byte| byte != b'-').collect();
    // Never echo the value: it may be a key.
    if digits.len() != 32 {
        return Err(Error::InvalidContentKey(format!(
            "expected 32 hexadecimal digits, got {}",
            digits.len()
        )));
    }
    let nibble = |digit: u8| {
        char::from(digit)
            .to_digit(16)
            .map(|value| value as u8)
            .ok_or_else(|| Error::InvalidContentKey("expected hexadecimal digits".to_owned()))
    };
    let mut out = [0; 16];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(out)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Why a body could not be decrypted, before the segment is attached.
#[derive(Debug)]
pub(crate) enum Problem {
    Malformed(String),
    Unsupported(String),
    MissingKey([u8; 16]),
}

impl Problem {
    pub(crate) fn at(self, location: &SegmentLocation) -> Error {
        let segment = location.to_string();
        match self {
            Self::Malformed(reason) => Error::MalformedProtection { segment, reason },
            Self::Unsupported(reason) => Error::UnsupportedProtection { segment, reason },
            Self::MissingKey(kid) => Error::MissingContentKey(hex(&kid)),
        }
    }
}

fn malformed(reason: impl Into<String>) -> Problem {
    Problem::Malformed(reason.into())
}

fn unsupported(reason: impl Into<String>) -> Problem {
    Problem::Unsupported(reason.into())
}

impl From<ParseError> for Problem {
    fn from(error: ParseError) -> Self {
        match error {
            ParseError::Malformed(reason) => Self::Malformed(reason),
            ParseError::Unsupported(reason) => Self::Unsupported(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    /// AES-128-CTR over each protected range.
    Cenc,
    /// AES-128-CBC with a crypt/skip block pattern, IV reset per range.
    Cbcs,
}

/// Encryption parameters for one protected audio track's samples, read from
/// its `tenc`.
#[derive(Debug)]
pub(crate) struct TrackProtection {
    track_id: u32,
    scheme: Scheme,
    pub(crate) kid: [u8; 16],
    /// `0` when every sample uses `constant_iv`.
    per_sample_iv_size: u8,
    constant_iv: [u8; 16],
    crypt_blocks: u8,
    skip_blocks: u8,
}

/// How a representation's media segments are protected, read from its init.
#[derive(Debug, Default)]
pub(crate) struct Protection {
    /// The protected tracks; empty when nothing needs decrypting.
    pub(crate) tracks: Vec<TrackProtection>,
    /// Every track's `trex` defaults, which lay out the samples of movie fragments.
    defaults: Vec<(u32, TrackDefaults)>,
}

/// A segment's role in decryption, set by the stream that requests it.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Debug)]
pub(crate) enum SegmentRole {
    /// The initialization segment: protected sample entries get their
    /// original types back.
    Init,
    /// A media segment of a representation whose init declared `protection`.
    Media(Arc<Protection>),
}

/// A sample entry with a `sinf`, located in an init body.
struct ProtectedEntry {
    /// Offset of the sample entry's 4-byte type.
    type_offset: usize,
    original_format: [u8; 4],
    /// `None` when `tenc` marks the track's samples as clear by default.
    protection: Option<TrackProtection>,
}

/// How the media of `init` is protected. Reads raw and cleared init bodies
/// alike, since clearing keeps `sinf`.
pub(crate) fn protection(init: &[u8]) -> Result<Protection, Problem> {
    let (entries, defaults) = protected_entries(init)?;
    Ok(Protection {
        tracks: entries
            .into_iter()
            .filter_map(|entry| entry.protection)
            .collect(),
        defaults,
    })
}

/// Restores each protected sample entry's original type, such as `fLaC` for
/// `enca`.
pub(crate) fn clear_sample_entries(init: &mut [u8]) -> Result<(), Problem> {
    for entry in protected_entries(init)?.0 {
        init[entry.type_offset..entry.type_offset + 4].copy_from_slice(&entry.original_format);
    }
    Ok(())
}

/// The protected sample entries of `init`'s audio tracks, and every track's `trex`
/// defaults. An init without a `moov` has neither.
type Entries = (Vec<ProtectedEntry>, Vec<(u32, TrackDefaults)>);

fn protected_entries(init: &[u8]) -> Result<Entries, Problem> {
    if find(atoms(init, 0, init.len()), b"moov")?.is_none() {
        return Ok((Vec::new(), Vec::new()));
    }
    let tracks = mp4::init::parse(init)?.tracks;
    let defaults = tracks
        .iter()
        .map(|track| (track.id, track.defaults))
        .collect();
    let mut entries = Vec::new();
    for track in tracks {
        if &track.handler != b"soun" {
            continue;
        }
        let before = entries.len();
        for entry in &track.entries {
            let Some(sinf) = entry.sinf(init)? else {
                continue;
            };
            let sinf = protection::sinf(init, sinf)?;
            entries.push(ProtectedEntry {
                type_offset: entry.type_offset(),
                original_format: sinf.original_format,
                protection: track_protection_from(&sinf, &track)?,
            });
        }
        if entries.len() - before > 1 {
            return Err(unsupported(format!(
                "track {} has several protected sample descriptions",
                track.id
            )));
        }
    }
    Ok((entries, defaults))
}

/// How `track`'s samples are protected under `sinf`, or `None` when its `tenc` marks
/// them clear by default.
fn track_protection_from(
    sinf: &protection::Sinf,
    track: &mp4::init::Track,
) -> Result<Option<TrackProtection>, Problem> {
    let scheme = match &sinf.scheme {
        b"cenc" => Scheme::Cenc,
        b"cbcs" => Scheme::Cbcs,
        other => return Err(unsupported(format!("{} scheme", fourcc(other)))),
    };
    let tenc = &sinf.tenc;
    if !tenc.is_protected {
        return Ok(None);
    }
    match scheme {
        Scheme::Cenc if tenc.crypt_blocks != 0 || tenc.skip_blocks != 0 => {
            return Err(unsupported("pattern encryption under the cenc scheme"));
        }
        Scheme::Cbcs if tenc.crypt_blocks == 0 && tenc.skip_blocks != 0 => {
            return Err(malformed("cbcs pattern skips blocks but encrypts none"));
        }
        _ => {}
    }
    Ok(Some(TrackProtection {
        track_id: track.id,
        scheme,
        kid: tenc.kid,
        per_sample_iv_size: tenc.per_sample_iv_size,
        constant_iv: tenc.constant_iv.unwrap_or_default(),
        crypt_blocks: tenc.crypt_blocks,
        skip_blocks: tenc.skip_blocks,
    }))
}

/// One protected sample's bytes in a media body and how to decrypt them.
struct Sample {
    range: Range<usize>,
    /// Index into [`Protection::tracks`].
    track: usize,
    iv: [u8; 16],
    /// `(clear, protected)` byte counts; empty when the whole sample is protected.
    subsamples: Vec<(usize, usize)>,
}

/// Decrypts every protected sample in each `moof`/`mdat` of `body`, in place.
/// `body_start` is the offset of `body` in its resource, such as the start of a
/// segment's byte range.
pub(crate) fn decrypt_segment(
    body: &mut [u8],
    body_start: u64,
    protection: &Protection,
    keys: &ContentKeys,
) -> Result<(), Problem> {
    let samples = protected_samples(body, body_start, protection)?;
    let tracks = &protection.tracks;
    let keys = tracks
        .iter()
        .map(|track| keys.key(&track.kid))
        .collect::<Result<Vec<_>, _>>()?;
    let ciphers: Vec<Aes128> = keys
        .iter()
        .map(|key| Aes128::new(&(**key).into()))
        .collect();
    for sample in samples {
        let track = &tracks[sample.track];
        let data = &mut body[sample.range];
        let regions = protected_regions(data.len(), &sample.subsamples)?;
        match track.scheme {
            Scheme::Cenc => {
                let mut ctr = Aes128Ctr64::new(&(*keys[sample.track]).into(), &sample.iv.into());
                for region in regions {
                    ctr.apply_keystream(&mut data[region]);
                }
            }
            Scheme::Cbcs => {
                for region in regions {
                    decrypt_cbcs(
                        &ciphers[sample.track],
                        &sample.iv,
                        track.crypt_blocks,
                        track.skip_blocks,
                        &mut data[region],
                    );
                }
            }
        }
    }
    Ok(())
}

/// Protected byte ranges of a `len`-byte sample from its subsample layout.
fn protected_regions(
    len: usize,
    subsamples: &[(usize, usize)],
) -> Result<Vec<Range<usize>>, Problem> {
    if subsamples.is_empty() {
        return Ok(std::iter::once(0..len).collect());
    }
    let mut pos = 0usize;
    let mut regions = Vec::with_capacity(subsamples.len());
    for &(clear, protected) in subsamples {
        let start = pos.checked_add(clear);
        let end = start.and_then(|start| start.checked_add(protected));
        let (Some(start), Some(end)) = (start, end.filter(|&end| end <= len)) else {
            return Err(malformed("subsamples exceed their sample"));
        };
        regions.push(start..end);
        pos = end;
    }
    if pos != len {
        return Err(malformed("subsamples do not cover their sample"));
    }
    Ok(regions)
}

/// Decrypts one `cbcs` protected range in place: CBC from `iv` over the
/// first `crypt` of every `crypt + skip` whole blocks, chaining across the
/// clear ones. A trailing partial block is clear. `0:0` encrypts every block.
fn decrypt_cbcs(aes: &Aes128, iv: &[u8; 16], crypt: u8, skip: u8, data: &mut [u8]) {
    let (crypt, period) = match crypt {
        0 => (1, 1),
        crypt => (usize::from(crypt), usize::from(crypt) + usize::from(skip)),
    };
    let mut chain = *iv;
    for (index, block) in data.chunks_exact_mut(BLOCK).enumerate() {
        if index % period >= crypt {
            continue;
        }
        let ciphertext: [u8; 16] = (*block).try_into().expect("exact chunk");
        let mut plain = ciphertext.into();
        aes.decrypt_block(&mut plain);
        for ((out, plain), chain) in block.iter_mut().zip(plain.iter()).zip(chain) {
            *out = plain ^ chain;
        }
        chain = ciphertext;
    }
}

/// The protected samples of every `moof` in `body`, which starts `body_start` bytes
/// into its resource.
fn protected_samples(
    body: &[u8],
    body_start: u64,
    protection: &Protection,
) -> Result<Vec<Sample>, Problem> {
    let mut samples = Vec::new();
    for traf in fragment::parse(body, body_start, &protection.defaults)? {
        let tracks = &protection.tracks;
        let Some(track_index) = tracks
            .iter()
            .position(|track| track.track_id == traf.track_id)
        else {
            continue;
        };
        let track = &tracks[track_index];
        for child in traf.atom.children(body) {
            let child = child?;
            if &child.kind == b"sgpd" {
                let mut sgpd = child.fields(body);
                sgpd.version_flags()?;
                if &sgpd.array::<4>()? == b"seig" {
                    return Err(unsupported("key rotation (seig sample groups)"));
                }
            }
        }

        let Some(senc) = find(traf.atom.children(body), b"senc")? else {
            if traf.samples.is_empty() {
                continue;
            }
            return Err(unsupported(
                "sample encryption information outside a senc box",
            ));
        };
        let entries = protection::senc(body, senc, track.per_sample_iv_size)?;
        if entries.len() != traf.samples.len() {
            return Err(malformed(format!(
                "senc describes {} samples, trun {}",
                entries.len(),
                traf.samples.len()
            )));
        }
        for (sample, entry) in traf.samples.into_iter().zip(entries) {
            samples.push(Sample {
                range: sample.range,
                track: track_index,
                iv: entry.iv.unwrap_or(track.constant_iv),
                subsamples: entry
                    .subsamples
                    .into_iter()
                    .map(|(clear, protected)| (usize::from(clear), protected as usize))
                    .collect(),
            });
        }
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::init::TrackDefaults;
    use crate::mp4::init::tests::{SOUND, TrackSpec, audio_entry, bx, full, init as mp4_init};
    use crate::mp4::protection::tests::sinf_box;
    use aes::cipher::BlockCipherEncrypt;

    const KID: [u8; 16] = *b"0123456789abcdef";
    const KEY: [u8; 16] = *b"fedcba9876543210";
    const TRACK_ID: u32 = 1;

    struct Tenc {
        scheme: &'static [u8; 4],
        version: u8,
        pattern: (u8, u8),
        iv_size: u8,
        constant_iv: Option<[u8; 16]>,
    }

    const CENC: Tenc = Tenc {
        scheme: b"cenc",
        version: 0,
        pattern: (0, 0),
        iv_size: 8,
        constant_iv: None,
    };

    fn cbcs(pattern: (u8, u8)) -> Tenc {
        Tenc {
            scheme: b"cbcs",
            version: 1,
            pattern,
            iv_size: 0,
            constant_iv: Some(*b"constant-iv-0123"),
        }
    }

    /// An init segment with one protected `fLaC` audio track.
    fn init(tenc: &Tenc, trex_sample_size: u32) -> Vec<u8> {
        let sinf = sinf_box(
            b"fLaC",
            tenc.scheme,
            (tenc.version == 1).then_some(tenc.pattern),
            tenc.iv_size,
            tenc.constant_iv.as_ref().map(|iv| &iv[..]),
            KID,
        );
        let entries = [audio_entry(
            b"enca",
            1,
            22_050,
            &[&bx(b"dfLa", &[&[0; 4]]), &sinf],
        )];
        mp4_init(&[TrackSpec {
            id: TRACK_ID,
            entries: &entries,
            trex: TrackDefaults {
                sample_description_index: 1,
                sample_duration: 0,
                sample_size: trex_sample_size,
                sample_flags: 0,
            },
            ..SOUND
        }])
    }

    struct EncryptedSample {
        data: Vec<u8>,
        iv: Vec<u8>,
        subsamples: Vec<(u16, u32)>,
    }

    struct Layout {
        /// Write sizes in `trun`; otherwise rely on the `trex` default.
        sizes_in_trun: bool,
        /// Split samples across two `trun`s, the second without a data offset.
        two_truns: bool,
        /// Where the movie fragment sits in its file, when `tfhd` names an explicit
        /// base data offset instead of using default-base-is-moof.
        file_offset: Option<u64>,
    }

    const SIZED: Layout = Layout {
        sizes_in_trun: true,
        two_truns: false,
        file_offset: None,
    };

    /// A `moof` + `mdat` movie fragment holding `samples`.
    fn movie_fragment(samples: &[EncryptedSample], layout: &Layout) -> Vec<u8> {
        let moof = |data_offset: i32| {
            let split = if layout.two_truns {
                samples.len() / 2
            } else {
                samples.len()
            };
            let trun = |part: &[EncryptedSample], offset: Option<i32>| {
                let mut flags = 0;
                let mut body = (part.len() as u32).to_be_bytes().to_vec();
                if let Some(offset) = offset {
                    flags |= 0x01;
                    body.extend(offset.to_be_bytes());
                }
                if layout.sizes_in_trun {
                    flags |= 0x200;
                    for sample in part {
                        body.extend((sample.data.len() as u32).to_be_bytes());
                    }
                }
                full(b"trun", 0, flags, &[&body])
            };
            let mut truns = trun(&samples[..split], Some(data_offset));
            if layout.two_truns {
                truns.extend(trun(&samples[split..], None));
            }
            let subsampled = samples.iter().any(|sample| !sample.subsamples.is_empty());
            let mut senc = (samples.len() as u32).to_be_bytes().to_vec();
            for sample in samples {
                senc.extend(&sample.iv);
                if subsampled {
                    senc.extend((sample.subsamples.len() as u16).to_be_bytes());
                    for &(clear, protected) in &sample.subsamples {
                        senc.extend(clear.to_be_bytes());
                        senc.extend(protected.to_be_bytes());
                    }
                }
            }
            let tfhd = match layout.file_offset {
                // The base is the moof's own start in the file.
                Some(offset) => full(
                    b"tfhd",
                    0,
                    0x01,
                    &[&TRACK_ID.to_be_bytes(), &offset.to_be_bytes()],
                ),
                None => full(b"tfhd", 0, 0x02_0000, &[&TRACK_ID.to_be_bytes()]),
            };
            let senc = full(b"senc", 0, if subsampled { 0x02 } else { 0 }, &[&senc]);
            let traf = bx(b"traf", &[&tfhd, &truns, &senc]);
            bx(
                b"moof",
                &[&full(b"mfhd", 0, 0, &[&1u32.to_be_bytes()]), &traf],
            )
        };
        let moof_len = moof(0).len();
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.data.clone())
            .collect();
        [moof(moof_len as i32 + 8), bx(b"mdat", &[&data])].concat()
    }

    fn aes() -> Aes128 {
        Aes128::new(&KEY.into())
    }

    fn encrypt_block(block: [u8; 16]) -> [u8; 16] {
        let mut block = block.into();
        aes().encrypt_block(&mut block);
        block.into()
    }

    fn regions(len: usize, subsamples: &[(u16, u32)]) -> Vec<Range<usize>> {
        if subsamples.is_empty() {
            return std::iter::once(0..len).collect();
        }
        let mut pos = 0;
        subsamples
            .iter()
            .map(|&(clear, protected)| {
                let start = pos + usize::from(clear);
                pos = start + protected as usize;
                start..pos
            })
            .collect()
    }

    /// Block-level AES-CTR over the concatenated protected ranges, with a
    /// 64-bit block counter in the IV's low half.
    fn cenc_reference(plain: &[u8], iv: [u8; 8], subsamples: &[(u16, u32)]) -> EncryptedSample {
        let mut data = plain.to_vec();
        let mut keystream = Vec::new();
        let mut counter = 0u64;
        for region in regions(data.len(), subsamples) {
            for byte in &mut data[region] {
                if keystream.is_empty() {
                    let mut block = [0; 16];
                    block[..8].copy_from_slice(&iv);
                    block[8..].copy_from_slice(&counter.to_be_bytes());
                    counter += 1;
                    keystream = encrypt_block(block).to_vec();
                    keystream.reverse();
                }
                *byte ^= keystream.pop().unwrap();
            }
        }
        EncryptedSample {
            data,
            iv: iv.to_vec(),
            subsamples: subsamples.to_vec(),
        }
    }

    /// Block-level AES-CBC over crypt runs of each protected range, every
    /// range restarting from `iv`, skipped blocks and a partial tail clear.
    fn cbcs_reference(
        plain: &[u8],
        iv: [u8; 16],
        (crypt, skip): (u8, u8),
        subsamples: &[(u16, u32)],
    ) -> EncryptedSample {
        let (crypt, skip) = if crypt == 0 { (1, 0) } else { (crypt, skip) };
        let mut data = plain.to_vec();
        for region in regions(data.len(), subsamples) {
            let range = &mut data[region];
            let blocks = range.len() / BLOCK;
            let mut chain = iv;
            let mut block = 0;
            while block < blocks {
                for _ in 0..crypt {
                    if block == blocks {
                        break;
                    }
                    let bytes = &mut range[block * BLOCK..][..BLOCK];
                    let mut input = [0; 16];
                    for (index, byte) in input.iter_mut().enumerate() {
                        *byte = bytes[index] ^ chain[index];
                    }
                    chain = encrypt_block(input);
                    bytes.copy_from_slice(&chain);
                    block += 1;
                }
                block += usize::from(skip);
            }
        }
        EncryptedSample {
            data,
            iv: Vec::new(),
            subsamples: subsamples.to_vec(),
        }
    }

    fn plaintext(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    fn keys() -> ContentKeys {
        let mut keys = ContentKeys::new();
        keys.insert(KID, KEY);
        keys
    }

    /// Clears `init`, then decrypts `media` with the protection it declares.
    fn decrypt(init: &mut [u8], media: &mut [u8]) -> Result<(), Problem> {
        clear_sample_entries(init)?;
        decrypt_segment(media, 0, &protection(init)?, &keys())
    }

    fn mdat(media: &[u8]) -> &[u8] {
        let at = media
            .windows(4)
            .position(|window| window == b"mdat")
            .unwrap();
        &media[at + 4..]
    }

    fn assert_decrypts(
        tenc: &Tenc,
        samples: &[EncryptedSample],
        plain: &[Vec<u8>],
        layout: &Layout,
    ) {
        let mut init = init(tenc, 0);
        let mut media = movie_fragment(samples, layout);
        let encrypted_len = media.len();
        assert_ne!(mdat(&media), plain.concat(), "fixture is not encrypted");
        decrypt(&mut init, &mut media).unwrap();
        assert_eq!(media.len(), encrypted_len);
        assert_eq!(mdat(&media), plain.concat());
    }

    #[test]
    fn merging_keeps_each_kids_key() {
        let mut known = keys();
        known.merge(keys()).unwrap();

        let other_kid = *b"another key id!!";
        let mut more = ContentKeys::new();
        more.insert(other_kid, KEY);
        known.merge(more).unwrap();
        assert!(known.contains(&other_kid));

        // Reject the entire set when it changes a known KID, including any new KIDs.
        let mut changed = ContentKeys::new();
        changed.insert(KID, *b"a different key!");
        changed.insert(*b"third key id....", KEY);
        assert_eq!(known.merge(changed), Err(KID));
        assert_eq!(known.key(&KID).unwrap(), &KEY);
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn hex_keys_accept_uuid_form_and_never_echo_values() {
        let mut keys = ContentKeys::new();
        keys.insert_hex(
            "01234567-89ab-cdef-0123-456789ABCDEF",
            "00112233445566778899aabbccddeeff",
        )
        .unwrap();
        let kid = parse_hex_128("0123456789abcdef0123456789abcdef").unwrap();
        assert!(keys.contains(&kid));
        assert!(!format!("{keys:?}").contains("00112233"));

        let short = ContentKeys::new()
            .insert_hex("0123", "secretsecret")
            .err()
            .unwrap();
        assert!(matches!(&short, Error::InvalidContentKey(reason) if !reason.contains("0123")));
        let not_hex = parse_hex_128("0123456789abcdef0123456789abcdeg")
            .err()
            .unwrap();
        assert!(matches!(not_hex, Error::InvalidContentKey(_)));
    }

    #[test]
    fn clearing_restores_the_sample_entry_and_keeps_protection_readable() {
        let raw = init(&CENC, 0);
        let mut cleared = raw.clone();
        clear_sample_entries(&mut cleared).unwrap();

        assert_eq!(cleared.len(), raw.len());
        let at = raw.windows(4).position(|window| window == b"enca").unwrap();
        assert_eq!(&cleared[at..at + 4], b"fLaC");
        let tracks = protection(&cleared).unwrap().tracks;
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].kid, KID);
        assert_eq!(tracks[0].scheme, Scheme::Cenc);

        // Idempotent, as for a cached init a second reader parses again.
        let mut again = cleared.clone();
        clear_sample_entries(&mut again).unwrap();
        assert_eq!(again, cleared);
    }

    #[test]
    fn cenc_whole_samples() {
        let plain = [plaintext(100, 1), plaintext(37, 2), plaintext(0, 3)];
        let samples: Vec<_> = plain
            .iter()
            .enumerate()
            .map(|(index, sample)| cenc_reference(sample, [index as u8 + 1; 8], &[]))
            .collect();
        assert_decrypts(&CENC, &samples, &plain, &SIZED);
    }

    #[test]
    fn cenc_keystream_continues_across_subsamples() {
        let plain = [plaintext(90, 4), plaintext(50, 5)];
        let samples = [
            cenc_reference(&plain[0], [7; 8], &[(5, 20), (10, 30), (25, 0)]),
            cenc_reference(&plain[1], [8; 8], &[(50, 0)]),
        ];
        assert_decrypts(&CENC, &samples, &plain, &SIZED);
    }

    #[test]
    fn cenc_sample_sizes_from_trex_across_two_truns() {
        let plain: Vec<_> = (0..4).map(|seed| plaintext(48, seed)).collect();
        let samples: Vec<_> = plain
            .iter()
            .map(|sample| cenc_reference(sample, [sample[0]; 8], &[]))
            .collect();
        let mut init = init(&CENC, 48);
        let layout = Layout {
            sizes_in_trun: false,
            two_truns: true,
            file_offset: None,
        };
        let mut media = movie_fragment(&samples, &layout);
        decrypt(&mut init, &mut media).unwrap();
        assert_eq!(mdat(&media), plain.concat());
    }

    #[test]
    fn explicit_base_data_offsets_count_from_the_file() {
        // A SegmentBase subsegment: this body is bytes 5000.. of its file, and its
        // tfhd names the moof's absolute offset.
        let plain = [plaintext(64, 1), plaintext(20, 2)];
        let samples: Vec<_> = plain
            .iter()
            .map(|sample| cenc_reference(sample, [sample[0]; 8], &[]))
            .collect();
        let layout = Layout {
            file_offset: Some(5000),
            ..SIZED
        };
        let mut init = init(&CENC, 0);
        clear_sample_entries(&mut init).unwrap();
        let protection = protection(&init).unwrap();

        let mut media = movie_fragment(&samples, &layout);
        decrypt_segment(&mut media, 5000, &protection, &keys()).unwrap();
        assert_eq!(mdat(&media), plain.concat());

        // Read as if the body started the file, the offset points past it.
        let mut media = movie_fragment(&samples, &layout);
        assert!(matches!(
            decrypt_segment(&mut media, 0, &protection, &keys()),
            Err(Problem::Malformed(_))
        ));
    }

    #[test]
    fn cbcs_pattern_resets_iv_per_subsample_and_leaves_tails_clear() {
        let tenc = cbcs((1, 9));
        let iv = tenc.constant_iv.unwrap();
        let plain = [plaintext(16 * 23 + 5, 6), plaintext(16 * 3 + 15, 7)];
        let samples = [
            cbcs_reference(&plain[0], iv, (1, 9), &[(3, 16 * 12 + 7), (9, 16 * 10 + 2)]),
            cbcs_reference(&plain[1], iv, (1, 9), &[(0, 16 * 3 + 15)]),
        ];
        assert_decrypts(&tenc, &samples, &plain, &SIZED);
    }

    #[test]
    fn cbcs_without_pattern_encrypts_every_whole_block() {
        let tenc = cbcs((0, 0));
        let iv = tenc.constant_iv.unwrap();
        let plain = [plaintext(16 * 40 + 9, 8), plaintext(15, 9)];
        let samples: Vec<_> = plain
            .iter()
            .map(|sample| cbcs_reference(sample, iv, (0, 0), &[]))
            .collect();
        assert_decrypts(&tenc, &samples, &plain, &SIZED);
    }

    #[test]
    fn missing_key_is_reported_by_kid() {
        let mut init = init(&CENC, 0);
        clear_sample_entries(&mut init).unwrap();
        let samples = [cenc_reference(&plaintext(32, 1), [1; 8], &[])];
        let mut media = movie_fragment(&samples, &SIZED);
        let problem = decrypt_segment(
            &mut media,
            0,
            &protection(&init).unwrap(),
            &ContentKeys::new(),
        );
        assert!(matches!(problem, Err(Problem::MissingKey(kid)) if kid == KID));
    }

    #[test]
    fn senc_count_must_match_trun() {
        let mut init = init(&CENC, 0);
        let samples = [
            cenc_reference(&plaintext(32, 1), [1; 8], &[]),
            cenc_reference(&plaintext(32, 2), [2; 8], &[]),
        ];
        let mut media = movie_fragment(&samples, &SIZED);
        // Rewrite senc's sample_count from 2 to 1.
        let at = media
            .windows(4)
            .position(|window| window == b"senc")
            .unwrap()
            + 8;
        media[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(
            decrypt(&mut init, &mut media),
            Err(Problem::Malformed(_))
        ));
    }

    #[test]
    fn subsamples_must_fit_their_sample() {
        let mut init = init(&CENC, 0);
        let mut sample = cenc_reference(&plaintext(32, 1), [1; 8], &[(16, 16)]);
        sample.subsamples = vec![(16, 17)];
        let mut media = movie_fragment(&[sample], &SIZED);
        assert!(matches!(
            decrypt(&mut init, &mut media),
            Err(Problem::Malformed(_))
        ));
    }

    #[test]
    fn subsamples_must_cover_their_sample() {
        let mut init = init(&CENC, 0);
        let mut sample = cenc_reference(&plaintext(32, 1), [1; 8], &[(16, 16)]);
        sample.subsamples = vec![(16, 8)];
        let mut media = movie_fragment(&[sample], &SIZED);
        assert!(matches!(
            decrypt(&mut init, &mut media),
            Err(Problem::Malformed(_))
        ));
    }

    #[test]
    fn unsupported_schemes_are_named() {
        let cens = Tenc {
            scheme: b"cens",
            ..CENC
        };
        assert!(matches!(
            protection(&init(&cens, 0)),
            Err(Problem::Unsupported(reason)) if reason.contains("cens")
        ));
        let patterned_cenc = Tenc {
            version: 1,
            pattern: (1, 9),
            ..CENC
        };
        assert!(matches!(
            protection(&init(&patterned_cenc, 0)),
            Err(Problem::Unsupported(_))
        ));
    }

    #[test]
    fn truncated_boxes_are_malformed_not_panics() {
        let full_init = init(&CENC, 0);
        for len in 0..full_init.len() {
            let mut prefix = full_init[..len].to_vec();
            let _ = clear_sample_entries(&mut prefix);
            let _ = protection(&prefix);
        }
        let mut cleared = full_init.clone();
        clear_sample_entries(&mut cleared).unwrap();
        let protection = protection(&cleared).unwrap();
        let media = movie_fragment(
            &[cenc_reference(&plaintext(40, 1), [1; 8], &[(3, 30)])],
            &SIZED,
        );
        for len in 0..media.len() {
            let mut prefix = media[..len].to_vec();
            let _ = decrypt_segment(&mut prefix, 0, &protection, &keys());
        }
    }

    #[test]
    fn unprotected_tracks_pass_through() {
        let plain = plaintext(64, 3);
        let mut media = movie_fragment(
            &[EncryptedSample {
                data: plain.clone(),
                iv: Vec::new(),
                subsamples: Vec::new(),
            }],
            &SIZED,
        );
        let before = media.clone();
        decrypt_segment(&mut media, 0, &Protection::default(), &keys()).unwrap();
        assert_eq!(media, before);
    }
}
