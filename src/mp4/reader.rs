//! ISO-BMFF boxes read from byte slices: box headers laid end to end, and the big-endian
//! fields of a box's payload. Every read is bounds-checked against its box, so malformed
//! input is an error, never a panic.

/// Why MP4 bytes could not be parsed.
#[derive(Debug)]
pub(crate) enum ParseError {
    /// Truncated or inconsistent boxes.
    Malformed(String),
    /// Valid MP4 that uses something this crate does not handle.
    Unsupported(String),
}

pub(crate) fn malformed(reason: impl Into<String>) -> ParseError {
    ParseError::Malformed(reason.into())
}

pub(crate) fn unsupported(reason: impl Into<String>) -> ParseError {
    ParseError::Unsupported(reason.into())
}

/// A four-character code as text, for messages.
pub(crate) fn fourcc(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

/// One ISO-BMFF box: its type and absolute offsets of its start, payload and end.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Atom {
    pub(crate) kind: [u8; 4],
    pub(crate) start: usize,
    pub(crate) payload: usize,
    pub(crate) end: usize,
}

impl Atom {
    /// The boxes in this box's payload.
    pub(crate) fn children(self, buf: &[u8]) -> Atoms<'_> {
        atoms(buf, self.payload, self.end)
    }

    /// A reader over this box's payload.
    pub(crate) fn fields(self, buf: &[u8]) -> Fields<'_> {
        Fields {
            buf,
            pos: self.payload,
            end: self.end,
            kind: self.kind,
        }
    }
}

/// Boxes laid end to end in `buf[pos..end]`.
pub(crate) struct Atoms<'a> {
    buf: &'a [u8],
    pos: usize,
    end: usize,
}

/// The boxes laid end to end in `buf[pos..end]`.
pub(crate) fn atoms(buf: &[u8], pos: usize, end: usize) -> Atoms<'_> {
    Atoms { buf, pos, end }
}

impl Atoms<'_> {
    fn read(&self) -> Result<Atom, ParseError> {
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
    type Item = Result<Atom, ParseError>;

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

/// The first of `atoms` of type `kind`.
pub(crate) fn find(atoms: Atoms<'_>, kind: &[u8; 4]) -> Result<Option<Atom>, ParseError> {
    for atom in atoms {
        let atom = atom?;
        if &atom.kind == kind {
            return Ok(Some(atom));
        }
    }
    Ok(None)
}

/// The first of `atoms` of type `kind`, which must be present.
pub(crate) fn require(atoms: Atoms<'_>, kind: &[u8; 4]) -> Result<Atom, ParseError> {
    find(atoms, kind)?.ok_or_else(|| malformed(format!("missing {} box", fourcc(kind))))
}

/// Big-endian fields read from one box's payload.
pub(crate) struct Fields<'a> {
    buf: &'a [u8],
    /// Absolute offset of the next field.
    pub(crate) pos: usize,
    end: usize,
    kind: [u8; 4],
}

impl<'a> Fields<'a> {
    /// A reader over `payload`, the whole payload of a `kind` box.
    pub(crate) fn of_payload(kind: [u8; 4], payload: &'a [u8]) -> Self {
        Self {
            buf: payload,
            pos: 0,
            end: payload.len(),
            kind,
        }
    }

    /// Bytes left to read.
    pub(crate) fn remaining(&self) -> usize {
        self.end - self.pos
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], ParseError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.end)
            .ok_or_else(|| malformed(format!("truncated {} box", fourcc(&self.kind))))?;
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], ParseError> {
        Ok(self.bytes(N)?.try_into().expect("length checked"))
    }

    pub(crate) fn skip(&mut self, len: usize) -> Result<(), ParseError> {
        self.bytes(len).map(drop)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, ParseError> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ParseError> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ParseError> {
        self.array().map(i32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ParseError> {
        self.array().map(u64::from_be_bytes)
    }

    /// A full box's version and 24-bit flags.
    pub(crate) fn version_flags(&mut self) -> Result<(u8, u32), ParseError> {
        let value = self.u32()?;
        Ok(((value >> 24) as u8, value & 0x00ff_ffff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(payload);
        out
    }

    #[test]
    fn walks_boxes_including_64_bit_and_to_end_sizes() {
        let mut body = bx(b"free", &[1, 2]);
        // size 1: a 64-bit size follows the type.
        body.extend(1u32.to_be_bytes());
        body.extend(b"wide");
        body.extend(19u64.to_be_bytes());
        body.extend([3, 4, 5]);
        // size 0: runs to the end of the parent.
        body.extend(0u32.to_be_bytes());
        body.extend(b"mdat");
        body.extend([6; 5]);

        let found: Vec<_> = atoms(&body, 0, body.len())
            .map(|atom| atom.unwrap())
            .map(|atom| (atom.kind, &body[atom.payload..atom.end]))
            .collect();
        assert_eq!(
            found,
            [
                (*b"free", &[1, 2][..]),
                (*b"wide", &[3, 4, 5][..]),
                (*b"mdat", &[6; 5][..])
            ]
        );
    }

    #[test]
    fn overruns_and_truncation_are_malformed() {
        let body = bx(b"moov", &[0; 4]);
        let overrun = atoms(&body, 0, body.len() - 1).next().unwrap();
        assert!(matches!(overrun, Err(ParseError::Malformed(reason)) if reason.contains("moov")));

        // A declared size smaller than the header.
        let mut tiny = 4u32.to_be_bytes().to_vec();
        tiny.extend(b"free");
        assert!(atoms(&tiny, 0, tiny.len()).next().unwrap().is_err());

        let atom = atoms(&body, 0, body.len()).next().unwrap().unwrap();
        let mut fields = atom.fields(&body);
        assert_eq!(fields.u32().unwrap(), 0);
        assert!(
            matches!(fields.u8(), Err(ParseError::Malformed(reason)) if reason == "truncated moov box")
        );
        // Four bytes are too few for a child's header.
        assert!(matches!(
            atom.children(&body).next().unwrap(),
            Err(ParseError::Malformed(reason)) if reason == "truncated head box"
        ));

        let empty = bx(b"moov", &[]);
        let atom = atoms(&empty, 0, empty.len()).next().unwrap().unwrap();
        assert!(matches!(
            require(atom.children(&empty), b"trak"),
            Err(ParseError::Malformed(reason)) if reason == "missing trak box"
        ));
    }
}
