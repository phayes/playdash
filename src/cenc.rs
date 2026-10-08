//! ISO/IEC 23001-7 Common Encryption for fragmented MP4 audio.
//!
//! Decrypts `cenc` (AES-128-CTR) and `cbcs` (AES-128-CBC with a crypt/skip
//! pattern) samples in place. Decryption never changes a body's length, so
//! fragment sizes from ranges and HEAD stay valid. The protection boxes
//! (`sinf`, `senc`, `saiz`, `saio`, `pssh`) stay where they are; only each
//! protected sample entry's type is restored from `enca` to its `frma`
//! original, so Symphonia recognizes the codec.

use crate::error::Error;
use crate::stream::Fragment;
use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit, KeyIvInit, StreamCipher};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
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
    /// An empty key set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `key` for `kid`, replacing any earlier key for that KID.
    pub fn insert(&mut self, kid: [u8; 16], key: [u8; 16]) -> &mut Self {
        self.keys.insert(kid, key);
        self
    }

    /// Adds a key given as hexadecimal, like mp4decrypt's `--key kid:key`.
    ///
    /// Each is 32 hex digits. Dashes are ignored, so a manifest's UUID-form
    /// `cenc:default_KID` works as is.
    pub fn insert_hex(&mut self, kid: &str, key: &str) -> Result<&mut Self, Error> {
        let kid = parse_hex_128(kid)?;
        let key = parse_hex_128(key)?;
        Ok(self.insert(kid, key))
    }

    /// Whether a key for `kid` is present.
    pub fn contains(&self, kid: &[u8; 16]) -> bool {
        self.keys.contains_key(kid)
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether there are no keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
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

/// Why a body could not be decrypted, before the fragment is attached.
#[derive(Debug)]
pub(crate) enum Problem {
    Malformed(String),
    Unsupported(String),
    MissingKey([u8; 16]),
}

impl Problem {
    pub(crate) fn at(self, fragment: &Fragment) -> Error {
        let fragment = fragment.to_string();
        match self {
            Self::Malformed(reason) => Error::MalformedProtection { fragment, reason },
            Self::Unsupported(reason) => Error::UnsupportedProtection { fragment, reason },
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

fn fourcc(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

/// One ISO-BMFF box: its type and absolute offsets of its start, payload and end.
#[derive(Clone, Copy, Debug)]
struct Atom {
    kind: [u8; 4],
    start: usize,
    payload: usize,
    end: usize,
}

impl Atom {
    fn children(self, buf: &[u8]) -> Atoms<'_> {
        atoms(buf, self.payload, self.end)
    }

    fn fields(self, buf: &[u8]) -> Fields<'_> {
        Fields {
            buf,
            pos: self.payload,
            end: self.end,
            kind: self.kind,
        }
    }
}

/// Boxes laid end to end in `buf[pos..end]`.
struct Atoms<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
}

fn atoms(buf: &[u8], pos: usize, end: usize) -> Atoms<'_> {
    Atoms { buf, pos, end }
}

impl Atoms<'_> {
    fn read(&self) -> Result<Atom, Problem> {
        let start = self.pos;
        let mut header = Fields {
            buf: self.buf,
            pos: start,
            end: self.end,
            kind: *b"head",
        };
        let size = header.u32()?;
        let kind = header.array::<4>()?;
        let end = match size {
            0 => Some(self.end),
            1 => usize::try_from(header.u64()?)
                .ok()
                .and_then(|size| start.checked_add(size)),
            size => start.checked_add(size as usize),
        };
        let payload = header.pos;
        end.filter(|&end| payload <= end && end <= self.end)
            .map(|end| Atom {
                kind,
                start,
                payload,
                end,
            })
            .ok_or_else(|| malformed(format!("{} box overruns its parent", fourcc(&kind))))
    }
}

impl Iterator for Atoms<'_> {
    type Item = Result<Atom, Problem>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end {
            return None;
        }
        let atom = self.read();
        // After an error, stop so the caller's `?` sees it once.
        self.pos = atom.as_ref().map_or(self.end, |atom| atom.end);
        Some(atom)
    }
}

fn find(atoms: Atoms<'_>, kind: &[u8; 4]) -> Result<Option<Atom>, Problem> {
    for atom in atoms {
        let atom = atom?;
        if &atom.kind == kind {
            return Ok(Some(atom));
        }
    }
    Ok(None)
}

fn require(atoms: Atoms<'_>, kind: &[u8; 4]) -> Result<Atom, Problem> {
    find(atoms, kind)?.ok_or_else(|| malformed(format!("missing {} box", fourcc(kind))))
}

/// Big-endian fields read from one box's payload.
struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
    kind: [u8; 4],
}

impl<'a> Fields<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], Problem> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.end)
            .ok_or_else(|| malformed(format!("truncated {} box", fourcc(&self.kind))))?;
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Problem> {
        Ok(self.bytes(N)?.try_into().expect("length checked"))
    }

    fn skip(&mut self, len: usize) -> Result<(), Problem> {
        self.bytes(len).map(drop)
    }

    fn u8(&mut self) -> Result<u8, Problem> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, Problem> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, Problem> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Result<i32, Problem> {
        self.array().map(i32::from_be_bytes)
    }

    fn u64(&mut self) -> Result<u64, Problem> {
        self.array().map(u64::from_be_bytes)
    }

    /// A full box's version and 24-bit flags.
    fn version_flags(&mut self) -> Result<(u8, u32), Problem> {
        let value = self.u32()?;
        Ok(((value >> 24) as u8, value & 0x00ff_ffff))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    /// AES-128-CTR over each protected range.
    Cenc,
    /// AES-128-CBC with a crypt/skip block pattern, IV reset per range.
    Cbcs,
}

/// How one protected audio track's samples are encrypted, from its `tenc`
/// and `trex`.
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
    /// `trex` default, for fragments whose `tfhd` and `trun` omit sizes.
    default_sample_size: u32,
}

/// What a fragment is to decryption, set by the reader that requests it.
#[derive(Debug)]
pub(crate) enum FragmentRole {
    /// The initialization segment: protected sample entries get their
    /// original types back.
    Init,
    /// A media fragment of a representation whose init declared `tracks`.
    Media(Arc<[TrackProtection]>),
}

/// A sample entry with a `sinf`, located in an init body.
struct ProtectedEntry {
    /// Offset of the sample entry's 4-byte type.
    type_offset: usize,
    original_format: [u8; 4],
    /// `None` when `tenc` marks the track's samples as clear by default.
    protection: Option<TrackProtection>,
}

/// Protection for each protected audio track in `init`. Reads raw and
/// cleared init bodies alike, since clearing keeps `sinf`.
pub(crate) fn track_protection(init: &[u8]) -> Result<Vec<TrackProtection>, Problem> {
    Ok(protected_entries(init)?
        .into_iter()
        .filter_map(|entry| entry.protection)
        .collect())
}

/// Restores each protected sample entry's original type, such as `fLaC` for
/// `enca`.
pub(crate) fn clear_sample_entries(init: &mut [u8]) -> Result<(), Problem> {
    for entry in protected_entries(init)? {
        init[entry.type_offset..entry.type_offset + 4].copy_from_slice(&entry.original_format);
    }
    Ok(())
}

fn protected_entries(init: &[u8]) -> Result<Vec<ProtectedEntry>, Problem> {
    let mut entries = Vec::new();
    for moov in atoms(init, 0, init.len()) {
        let moov = moov?;
        if &moov.kind != b"moov" {
            continue;
        }
        let sample_sizes = trex_sample_sizes(init, moov)?;
        for trak in moov.children(init) {
            let trak = trak?;
            if &trak.kind == b"trak" {
                track_entries(init, trak, &sample_sizes, &mut entries)?;
            }
        }
    }
    Ok(entries)
}

/// `trex` default sample sizes by track ID.
fn trex_sample_sizes(init: &[u8], moov: Atom) -> Result<HashMap<u32, u32>, Problem> {
    let mut sizes = HashMap::new();
    let Some(mvex) = find(moov.children(init), b"mvex")? else {
        return Ok(sizes);
    };
    for trex in mvex.children(init) {
        let trex = trex?;
        if &trex.kind != b"trex" {
            continue;
        }
        let mut fields = trex.fields(init);
        fields.version_flags()?;
        let track_id = fields.u32()?;
        // default_sample_description_index, default_sample_duration
        fields.skip(8)?;
        sizes.insert(track_id, fields.u32()?);
    }
    Ok(sizes)
}

fn track_entries(
    init: &[u8],
    trak: Atom,
    sample_sizes: &HashMap<u32, u32>,
    entries: &mut Vec<ProtectedEntry>,
) -> Result<(), Problem> {
    let mut tkhd = require(trak.children(init), b"tkhd")?.fields(init);
    let (version, _) = tkhd.version_flags()?;
    // creation and modification times
    tkhd.skip(if version == 1 { 16 } else { 8 })?;
    let track_id = tkhd.u32()?;

    let mdia = require(trak.children(init), b"mdia")?;
    let mut hdlr = require(mdia.children(init), b"hdlr")?.fields(init);
    hdlr.version_flags()?;
    hdlr.skip(4)?;
    if &hdlr.array::<4>()? != b"soun" {
        return Ok(());
    }
    let minf = require(mdia.children(init), b"minf")?;
    let stbl = require(minf.children(init), b"stbl")?;
    let stsd = require(stbl.children(init), b"stsd")?;
    let mut fields = stsd.fields(init);
    fields.version_flags()?;
    fields.u32()?;

    let before = entries.len();
    for entry in atoms(init, fields.pos, stsd.end) {
        let entry = entry?;
        // AudioSampleEntry: an 8-byte SampleEntry header and 20 bytes of
        // audio fields, which QuickTime sound description versions 1 and 2
        // extend before the child boxes.
        let mut fields = entry.fields(init);
        fields.skip(8)?;
        let extension = match fields.u16()? {
            0 => 0,
            1 => 16,
            2 => 36,
            version => return Err(unsupported(format!("audio sample entry version {version}"))),
        };
        let children = entry.payload + 28 + extension;
        if children > entry.end {
            return Err(malformed(format!("truncated {} box", fourcc(&entry.kind))));
        }
        let Some(sinf) = find(atoms(init, children, entry.end), b"sinf")? else {
            continue;
        };
        let default_sample_size = sample_sizes.get(&track_id).copied().unwrap_or(0);
        let mut protected = parse_sinf(init, sinf, track_id, default_sample_size)?;
        protected.type_offset = entry.start + 4;
        entries.push(protected);
    }
    if entries.len() - before > 1 {
        return Err(unsupported(format!(
            "track {track_id} has several protected sample descriptions"
        )));
    }
    Ok(())
}

fn parse_sinf(
    init: &[u8],
    sinf: Atom,
    track_id: u32,
    default_sample_size: u32,
) -> Result<ProtectedEntry, Problem> {
    let original_format = require(sinf.children(init), b"frma")?
        .fields(init)
        .array::<4>()?;

    let mut schm = require(sinf.children(init), b"schm")?.fields(init);
    schm.version_flags()?;
    let scheme = match &schm.array::<4>()? {
        b"cenc" => Scheme::Cenc,
        b"cbcs" => Scheme::Cbcs,
        other => return Err(unsupported(format!("{} scheme", fourcc(other)))),
    };

    let schi = require(sinf.children(init), b"schi")?;
    let mut tenc = require(schi.children(init), b"tenc")?.fields(init);
    let (version, _) = tenc.version_flags()?;
    tenc.skip(1)?;
    // Version 0 has a reserved byte where version 1 has the pattern.
    let pattern = tenc.u8()?;
    let (crypt_blocks, skip_blocks) = match version {
        0 => (0, 0),
        _ => (pattern >> 4, pattern & 0x0f),
    };
    let is_protected = tenc.u8()? != 0;
    let per_sample_iv_size = tenc.u8()?;
    let kid = tenc.array::<16>()?;

    let mut entry = ProtectedEntry {
        type_offset: 0,
        original_format,
        protection: None,
    };
    if !is_protected {
        return Ok(entry);
    }
    let mut constant_iv = [0; 16];
    match per_sample_iv_size {
        0 => {
            let size = usize::from(tenc.u8()?);
            if !matches!(size, 8 | 16) {
                return Err(malformed(format!("{size}-byte constant IV")));
            }
            constant_iv[..size].copy_from_slice(tenc.bytes(size)?);
        }
        8 | 16 => {}
        size => return Err(malformed(format!("{size}-byte per-sample IV"))),
    }
    match scheme {
        Scheme::Cenc if crypt_blocks != 0 || skip_blocks != 0 => {
            return Err(unsupported("pattern encryption under the cenc scheme"));
        }
        Scheme::Cbcs if crypt_blocks == 0 && skip_blocks != 0 => {
            return Err(malformed("cbcs pattern skips blocks but encrypts none"));
        }
        _ => {}
    }
    entry.protection = Some(TrackProtection {
        track_id,
        scheme,
        kid,
        per_sample_iv_size,
        constant_iv,
        crypt_blocks,
        skip_blocks,
        default_sample_size,
    });
    Ok(entry)
}

/// One protected sample's bytes in a media body and how to decrypt them.
struct Sample {
    range: Range<usize>,
    /// Index into the fragment's track list.
    track: usize,
    iv: [u8; 16],
    /// `(clear, protected)` byte counts; empty when the whole sample is protected.
    subsamples: Vec<(usize, usize)>,
}

/// Decrypts every sample of `tracks` in each `moof`/`mdat` of `body`, in place.
pub(crate) fn decrypt_fragment(
    body: &mut [u8],
    tracks: &[TrackProtection],
    keys: &ContentKeys,
) -> Result<(), Problem> {
    let samples = protected_samples(body, tracks)?;
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

fn protected_samples(body: &[u8], tracks: &[TrackProtection]) -> Result<Vec<Sample>, Problem> {
    let mut samples = Vec::new();
    for moof in atoms(body, 0, body.len()) {
        let moof = moof?;
        if &moof.kind != b"moof" {
            continue;
        }
        // A traf without an explicit base starts at the moof if first, else
        // where the previous traf's data ended.
        let mut implicit_base = Some(moof.start);
        for traf in moof.children(body) {
            let traf = traf?;
            if &traf.kind == b"traf" {
                implicit_base =
                    traf_samples(body, moof, traf, implicit_base, tracks, &mut samples)?;
            }
        }
    }
    Ok(samples)
}

/// Appends the protected samples of one `traf`. Returns where its data ends,
/// or `None` for a track this fragment does not decrypt.
fn traf_samples(
    body: &[u8],
    moof: Atom,
    traf: Atom,
    implicit_base: Option<usize>,
    tracks: &[TrackProtection],
    samples: &mut Vec<Sample>,
) -> Result<Option<usize>, Problem> {
    let mut tfhd = require(traf.children(body), b"tfhd")?.fields(body);
    let (_, flags) = tfhd.version_flags()?;
    let track_id = tfhd.u32()?;
    let Some(track_index) = tracks.iter().position(|track| track.track_id == track_id) else {
        return Ok(None);
    };
    let track = &tracks[track_index];
    let explicit_base = if flags & 0x01 != 0 {
        // Relative to the body, which is the file for whole-segment fetches.
        Some(usize::try_from(tfhd.u64()?).map_err(|_| malformed("base data offset overflows"))?)
    } else {
        None
    };
    if flags & 0x02 != 0 {
        // sample_description_index
        tfhd.skip(4)?;
    }
    if flags & 0x08 != 0 {
        // default_sample_duration
        tfhd.skip(4)?;
    }
    let default_size = if flags & 0x10 != 0 {
        tfhd.u32()?
    } else {
        track.default_sample_size
    };
    let base = match explicit_base {
        Some(base) => base,
        None if flags & 0x02_0000 != 0 => moof.start,
        None => implicit_base
            .ok_or_else(|| unsupported("implicit data offset after an undecrypted track"))?,
    };

    let mut ranges = Vec::new();
    let mut data_end = base;
    for child in traf.children(body) {
        let child = child?;
        match &child.kind {
            b"trun" => {
                data_end = trun_ranges(body, child, base, data_end, default_size, &mut ranges)?
            }
            b"sgpd" => {
                let mut sgpd = child.fields(body);
                sgpd.version_flags()?;
                if &sgpd.array::<4>()? == b"seig" {
                    return Err(unsupported("key rotation (seig sample groups)"));
                }
            }
            _ => {}
        }
    }

    let Some(senc) = find(traf.children(body), b"senc")? else {
        if ranges.is_empty() {
            return Ok(Some(data_end));
        }
        return Err(unsupported(
            "sample encryption information outside a senc box",
        ));
    };
    let mut senc = senc.fields(body);
    let (_, senc_flags) = senc.version_flags()?;
    let count = senc.u32()? as usize;
    if count != ranges.len() {
        return Err(malformed(format!(
            "senc describes {count} samples, trun {}",
            ranges.len()
        )));
    }
    let iv_size = usize::from(track.per_sample_iv_size);
    for range in ranges {
        let mut iv = track.constant_iv;
        if iv_size != 0 {
            iv = [0; 16];
            iv[..iv_size].copy_from_slice(senc.bytes(iv_size)?);
        }
        let mut subsamples = Vec::new();
        if senc_flags & 0x02 != 0 {
            for _ in 0..senc.u16()? {
                subsamples.push((usize::from(senc.u16()?), senc.u32()? as usize));
            }
        }
        samples.push(Sample {
            range,
            track: track_index,
            iv,
            subsamples,
        });
    }
    Ok(Some(data_end))
}

/// Appends the byte ranges of one `trun`'s samples. Returns where they end.
fn trun_ranges(
    body: &[u8],
    trun: Atom,
    base: usize,
    previous_end: usize,
    default_size: u32,
    ranges: &mut Vec<Range<usize>>,
) -> Result<usize, Problem> {
    let mut fields = trun.fields(body);
    let (_, flags) = fields.version_flags()?;
    let count = fields.u32()? as usize;
    // Samples may be empty, but not more numerous than the body's bytes.
    if count > body.len() {
        return Err(malformed(format!("trun claims {count} samples")));
    }
    let mut pos = if flags & 0x01 != 0 {
        let offset = isize::try_from(fields.i32()?).expect("i32 fits isize");
        base.checked_add_signed(offset)
            .ok_or_else(|| malformed("trun data offset precedes the body"))?
    } else {
        previous_end
    };
    if flags & 0x04 != 0 {
        // first_sample_flags
        fields.skip(4)?;
    }
    for _ in 0..count {
        if flags & 0x100 != 0 {
            // sample_duration
            fields.skip(4)?;
        }
        let size = if flags & 0x200 != 0 {
            fields.u32()?
        } else {
            default_size
        };
        if flags & 0x400 != 0 {
            // sample_flags
            fields.skip(4)?;
        }
        if flags & 0x800 != 0 {
            // sample_composition_time_offset
            fields.skip(4)?;
        }
        let end = pos
            .checked_add(size as usize)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| malformed("sample data extends past the fragment"))?;
        ranges.push(pos..end);
        pos = end;
    }
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockCipherEncrypt;

    const KID: [u8; 16] = *b"0123456789abcdef";
    const KEY: [u8; 16] = *b"fedcba9876543210";
    const TRACK_ID: u32 = 1;

    fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
        let len = 8 + parts.iter().map(|part| part.len()).sum::<usize>();
        let mut out = (len as u32).to_be_bytes().to_vec();
        out.extend(kind);
        for part in parts {
            out.extend(*part);
        }
        out
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, parts: &[&[u8]]) -> Vec<u8> {
        let header = (u32::from(version) << 24 | flags).to_be_bytes();
        let mut all: Vec<&[u8]> = vec![&header];
        all.extend(parts);
        bx(kind, &all)
    }

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
        let (crypt, skip) = tenc.pattern;
        let mut tenc_body = vec![0, crypt << 4 | skip, 1, tenc.iv_size];
        tenc_body.extend(KID);
        if let Some(iv) = tenc.constant_iv {
            tenc_body.push(16);
            tenc_body.extend(iv);
        }
        let sinf = bx(
            b"sinf",
            &[
                &bx(b"frma", &[b"fLaC"]),
                &full(b"schm", 0, 0, &[tenc.scheme, &0x1_0000u32.to_be_bytes()]),
                &bx(b"schi", &[&full(b"tenc", tenc.version, 0, &[&tenc_body])]),
            ],
        );
        let mut audio_fields = [0u8; 28];
        audio_fields[7] = 1; // data_reference_index
        let enca = bx(b"enca", &[&audio_fields, &bx(b"dfLa", &[&[0; 4]]), &sinf]);
        let stsd = full(b"stsd", 0, 0, &[&1u32.to_be_bytes(), &enca]);
        let minf = bx(b"minf", &[&bx(b"stbl", &[&stsd])]);
        let hdlr = full(b"hdlr", 0, 0, &[&[0; 4], b"soun", &[0; 13]]);
        let tkhd = full(b"tkhd", 0, 3, &[&[0; 8], &TRACK_ID.to_be_bytes(), &[0; 68]]);
        let trak = bx(b"trak", &[&tkhd, &bx(b"mdia", &[&hdlr, &minf])]);
        let trex = full(
            b"trex",
            0,
            0,
            &[
                &TRACK_ID.to_be_bytes(),
                &1u32.to_be_bytes(),
                &0u32.to_be_bytes(),
                &trex_sample_size.to_be_bytes(),
                &0u32.to_be_bytes(),
            ],
        );
        let moov = bx(b"moov", &[&trak, &bx(b"mvex", &[&trex])]);
        [bx(b"ftyp", &[b"iso6", &[0; 4]]), moov].concat()
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
    }

    const SIZED: Layout = Layout {
        sizes_in_trun: true,
        two_truns: false,
    };

    /// A `moof` + `mdat` media fragment holding `samples`.
    fn fragment(samples: &[EncryptedSample], layout: &Layout) -> Vec<u8> {
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
            let tfhd = full(b"tfhd", 0, 0x02_0000, &[&TRACK_ID.to_be_bytes()]);
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
        decrypt_fragment(media, &track_protection(init)?, &keys())
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
        let mut media = fragment(samples, layout);
        let encrypted_len = media.len();
        assert_ne!(mdat(&media), plain.concat(), "fixture is not encrypted");
        decrypt(&mut init, &mut media).unwrap();
        assert_eq!(media.len(), encrypted_len);
        assert_eq!(mdat(&media), plain.concat());
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
        let tracks = track_protection(&cleared).unwrap();
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
        };
        let mut media = fragment(&samples, &layout);
        decrypt(&mut init, &mut media).unwrap();
        assert_eq!(mdat(&media), plain.concat());
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
        let mut media = fragment(&samples, &SIZED);
        let problem = decrypt_fragment(
            &mut media,
            &track_protection(&init).unwrap(),
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
        let mut media = fragment(&samples, &SIZED);
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
        let mut media = fragment(&[sample], &SIZED);
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
            track_protection(&init(&cens, 0)),
            Err(Problem::Unsupported(reason)) if reason.contains("cens")
        ));
        let patterned_cenc = Tenc {
            version: 1,
            pattern: (1, 9),
            ..CENC
        };
        assert!(matches!(
            track_protection(&init(&patterned_cenc, 0)),
            Err(Problem::Unsupported(_))
        ));
    }

    #[test]
    fn truncated_boxes_are_malformed_not_panics() {
        let full_init = init(&CENC, 0);
        for len in 0..full_init.len() {
            let mut prefix = full_init[..len].to_vec();
            let _ = clear_sample_entries(&mut prefix);
            let _ = track_protection(&prefix);
        }
        let mut cleared = full_init.clone();
        clear_sample_entries(&mut cleared).unwrap();
        let tracks = track_protection(&cleared).unwrap();
        let media = fragment(
            &[cenc_reference(&plaintext(40, 1), [1; 8], &[(3, 30)])],
            &SIZED,
        );
        for len in 0..media.len() {
            let mut prefix = media[..len].to_vec();
            let _ = decrypt_fragment(&mut prefix, &tracks, &keys());
        }
    }

    #[test]
    fn unprotected_tracks_pass_through() {
        let plain = plaintext(64, 3);
        let mut media = fragment(
            &[EncryptedSample {
                data: plain.clone(),
                iv: Vec::new(),
                subsamples: Vec::new(),
            }],
            &SIZED,
        );
        let before = media.clone();
        decrypt_fragment(&mut media, &[], &keys()).unwrap();
        assert_eq!(media, before);
    }
}
