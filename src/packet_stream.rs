//! A representation's packets: the blocking `PacketStream` over a `FragmentStream`.

use crate::buffer::BufferHandle;
use crate::decoder::{Choice, DecoderFormat, DecoderStream, DecoderStyle};
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::demux::{Demuxer, Loaded, Split};
use crate::mp4::packet::{Packet, Seeked};
use crate::schedule::{Part, PartBody, TimeAccuracy};
use crate::stream::FragmentStream;
use std::time::Duration;

/// A representation's coded frames, demuxed from the fragmented MP4 a
/// [`FragmentStream`] downloads: an [`Iterator`] of [`Packet`]s in decode order, with
/// time seeking. The blocking counterpart of the `async` feature's
/// `nonblocking::PacketStream`.
///
/// It reads one track: a representation's only one, or one chosen from
/// [`FragmentStream::tracks`]. Protected media must come through a
/// [`FragmentCache`](crate::FragmentCache) with the content keys.
///
/// After an error, iteration yields it once and then ends, until a seek succeeds.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use playdash::{DashManifest, PacketStream, TimeAccuracy};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let mut packets = PacketStream::new(manifest.stream("audio")?)?;
/// println!("{:?}", packets.codec());
///
/// // Prime a decoder with the pre-roll, then decode from the landing point.
/// let seeked = packets.seek_time(Duration::from_secs(30), TimeAccuracy::Packet)?;
/// for packet in seeked.preroll {
///     # let _ = packet;
///     // decode, and discard the output
/// }
/// for packet in packets {
///     let packet = packet?;
///     // decode, discarding output presented before seeked.discard_before
///     # let _ = packet;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct PacketStream {
    fragments: FragmentStream,
    demuxer: Demuxer,
    /// Set once iteration has yielded an error, so it ends rather than
    /// yielding the same error forever.
    failed: bool,
}

impl PacketStream {
    /// Reads the packets of `fragments`, a representation of one track, which must not
    /// have been read from yet. Reads its initialization fragment before returning.
    ///
    /// Fails with [`Error::UnsupportedMedia`] if the representation has more or fewer
    /// than one track, for which see [`Self::with_track`], or its samples are still
    /// encrypted. Fails with [`Error::MalformedMedia`] for a damaged initialization
    /// fragment, and with [`Error::Initialization`] if `fragments` has been read from.
    pub fn new(fragments: FragmentStream) -> Result<Self, Error> {
        Self::open(fragments, None)
    }

    /// Reads the packets of track `track_id` of `fragments`, one of its
    /// [`FragmentStream::tracks`]. Otherwise like [`Self::new`].
    ///
    /// Fails with [`Error::Initialization`] if the representation has no such track.
    pub fn with_track(fragments: FragmentStream, track_id: u32) -> Result<Self, Error> {
        Self::open(fragments, Some(track_id))
    }

    /// A stream of `fragments`' track `track_id`, or of its only track without one.
    fn open(mut fragments: FragmentStream, track_id: Option<u32>) -> Result<Self, Error> {
        if !fragments.at_start() {
            return Err(Error::Initialization(
                "PacketStream needs a FragmentStream that has not been read from".to_owned(),
            ));
        }
        let init = match fragments.next_part() {
            Some(Ok(init)) if init.part == Part::Init => init,
            Some(Err(error)) => return Err(error),
            _ => {
                return Err(Error::Initialization(
                    "MPEG stream ended before its initialization fragment".to_owned(),
                ));
            }
        };
        let timeline = fragments.representation_timeline().cloned();
        let demuxer = Demuxer::new(init.body, &init.fragment, timeline, track_id)?;
        Ok(Self {
            fragments,
            demuxer,
            failed: false,
        })
    }

    /// Decodes `preroll` of media before a seek target, instead of what the media or
    /// [`Codec::default_preroll`] says.
    pub fn with_preroll(mut self, preroll: Duration) -> Self {
        self.demuxer.set_preroll(preroll);
        self
    }

    /// The track's codec and decoder configuration.
    pub fn codec(&self) -> &Codec {
        self.demuxer.codec()
    }

    /// Ticks per second of packet timestamps.
    pub fn timescale(&self) -> u32 {
        self.demuxer.timescale()
    }

    /// The MP4 track ID of the track read.
    pub fn track_id(&self) -> u32 {
        self.demuxer.track_id()
    }

    /// Media decoded before a seek target: the one [`Self::with_preroll`] set,
    /// else the `roll` or `prol` sample group of the initialization fragment,
    /// else [`Codec::default_preroll`]. A media fragment's own roll group can
    /// change it for seeks landing there.
    pub fn preroll(&self) -> Duration {
        self.demuxer.preroll()
    }

    /// A handle reporting how much of the representation the cache holds; see
    /// [`FragmentStream::buffer`].
    pub fn buffer(&self) -> Option<BufferHandle> {
        self.fragments.buffer()
    }

    /// Seeks to `time` on the representation's media timeline, and returns the
    /// packets to decode before the stream's next one.
    ///
    /// With [`TimeAccuracy::Packet`], the stream's next packet is the first one in
    /// decode order that presents at or after `time`. Pre-roll may reach into earlier
    /// fragments, which are fetched without moving the underlying stream's retention
    /// window. With [`TimeAccuracy::Fragment`], the next packet is the first one of
    /// the fragment containing `time` past that fragment's own pre-roll, and earlier
    /// fragments are only fetched if the fragment does not start at a keyframe.
    ///
    /// Fails like [`FragmentStream::seek_time`], and with the errors of fetching
    /// and parsing the fragments it reads.
    pub fn seek_time(&mut self, time: Duration, accuracy: TimeAccuracy) -> Result<Seeked, Error> {
        let range = self.demuxer.seek_range(time, accuracy)?;
        let first = self.fragments.first_index();

        // The fragment containing `time`, where the stream then carries on.
        self.fragments.seek_time(time)?;
        let target = match self.fragments.next_part() {
            Some(Ok(target)) => target,
            Some(Err(error)) => return Err(error),
            None => {
                return Err(Error::Initialization(
                    "MPEG stream ended before the requested media time".to_owned(),
                ));
            }
        };

        // Earlier fragments, fetched without moving the stream.
        let mut loaded = Vec::new();
        for index in (*range.start()).max(first)..*range.end() {
            let earlier = self.fragments.media_part_body(index)?;
            loaded.push(self.load(earlier)?);
        }
        loaded.push(self.load(target)?);

        // Back further until decoding can start, and on until a packet presents at the time.
        let mut at_last = false;
        loop {
            let earliest = loaded[0].index();
            let at_first = earliest <= first;
            match self
                .demuxer
                .seek(time, accuracy, &loaded, at_first, at_last)
            {
                Split::Done(seeked) => {
                    self.failed = false;
                    return Ok(seeked);
                }
                Split::Earlier => {
                    let earlier = self.fragments.media_part_body(earliest - 1)?;
                    loaded.insert(0, self.load(earlier)?);
                }
                // The stream's next fragment, where it then carries on.
                Split::Later => match self.fragments.next_part() {
                    Some(Ok(later)) => loaded.push(self.load(later)?),
                    Some(Err(error)) => return Err(error),
                    None => at_last = true,
                },
            }
        }
    }

    /// Converts this stream to one of decoder-ready packets in its codec's elementary
    /// format, such as [`DecoderFormat::H264AnnexB`] for H.264. The same as
    /// [`Self::into_decoder_stream_for`] with [`DecoderStyle::Elementary`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if the codec has no decoder-ready
    /// form, and with [`Error::MalformedDecoderInput`] if its MP4 decoder configuration
    /// is damaged.
    pub fn into_decoder_stream(self) -> Result<DecoderStream, Error> {
        self.into_decoder_stream_for(DecoderStyle::Elementary)
    }

    /// Converts this stream to one of decoder-ready packets in its codec's format of
    /// `style`, such as [`DecoderFormat::H264LengthPrefixed`] for H.264 in
    /// [`DecoderStyle::ConfigRecord`].
    ///
    /// Fails like [`Self::into_decoder_stream`].
    pub fn into_decoder_stream_for(self, style: DecoderStyle) -> Result<DecoderStream, Error> {
        DecoderStream::new(self, Choice::Style(style))
    }

    /// Converts this stream to one of decoder-ready packets in `format`.
    ///
    /// Fails like [`Self::into_decoder_stream`], and with
    /// [`Error::UnsupportedDecoderInput`] if the codec cannot be converted to `format`.
    pub fn into_decoder_stream_as(self, format: DecoderFormat) -> Result<DecoderStream, Error> {
        DecoderStream::new(self, Choice::Format(format))
    }

    /// The packets of a media fragment `part`.
    fn load(&mut self, part: PartBody) -> Result<Loaded, Error> {
        let index = part.index.expect("a media fragment");
        self.demuxer.load(index, &part.body, &part.fragment)
    }
}

impl Iterator for PacketStream {
    type Item = Result<Packet, Error>;

    /// The next packet in decode order. Blocks while its fragment downloads.
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(packet) = self.demuxer.pop() {
                return Some(Ok(packet));
            }
            let loaded = match self.fragments.next_part()? {
                Ok(part) => self.load(part),
                Err(error) => Err(error),
            };
            match loaded {
                Ok(loaded) => self.demuxer.push(loaded),
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}
