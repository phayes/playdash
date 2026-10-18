//! Errors returned by DASH parsing and segment streaming.

/// Errors that can occur when parsing a DASH manifest or streaming segments.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Manifest was not a base64-encoded `data:` URL
    #[error("Invalid DASH manifest data URL")]
    InvalidDataUrl,

    /// Base64-encoded data URL could not be decoded
    #[error("DASH manifest base64 decoding failed: {0}")]
    DataUrlBase64(#[from] base64::DecodeError),

    /// The fetched or decoded manifest contains invalid UTF-8 text.
    #[error("DASH manifest UTF-8 decoding failed: {0}")]
    DataUrlUtf8(#[from] std::string::FromUtf8Error),

    /// MPEG-DASH manifest XML could not be parsed
    #[error("MPEG-DASH manifest parsing failed: {0}")]
    DashManifestParse(#[from] crate::vendor::dash_mpd_core::DashMpdError),

    /// An MPEG-DASH representation did not include an initialization URL
    #[error("MPEG-DASH representation is missing an initialization URL")]
    DashManifestMissingUrls,

    /// MPEG-DASH manifest did not contain any representations
    #[error("MPEG-DASH manifest contains no representations")]
    DashManifestMissingRepresentations,

    /// An MPEG-DASH representation did not have an ID
    #[error("MPEG-DASH representation is missing its ID")]
    DashManifestMissingRepresentationId,

    /// No MPEG-DASH representation matched the requested ID or format token
    #[error("MPEG-DASH representation not found: {0}")]
    DashManifestMissingRepresentation(String),

    /// A MPEG-DASH representation had neither a SegmentTimeline nor a
    /// SegmentTemplate@duration, or its SegmentTimeline was empty
    #[error(
        "MPEG-DASH representation has neither a SegmentTimeline nor a SegmentTemplate@duration"
    )]
    DashManifestMissingTimeline,

    /// The segment count depends on the Period length, but the manifest gives
    /// no Period@duration, next Period@start, MPD@mediaPresentationDuration or
    /// SegmentTemplate@endNumber
    #[error("MPEG-DASH segment count needs a Period duration, presentation duration or endNumber")]
    DashManifestMissingDuration,

    /// A MPEG-DASH SegmentTemplate or SegmentList could not be expanded into segments
    #[error("Invalid MPEG-DASH segment addressing: {0}")]
    DashManifestInvalidSegments(String),

    /// A live manifest omits `MPD@availabilityStartTime`, so segment
    /// availability cannot be mapped to wall-clock time.
    #[error("Live MPEG-DASH manifest is missing MPD@availabilityStartTime")]
    DashManifestMissingAvailabilityStart,

    /// The live manifest uses a feature that this crate cannot play, or its
    /// segments were opened somewhere that cannot keep them refreshed, such as
    /// `SegmentStream::new` or rodio's `DashSource`.
    #[error("Unsupported live MPEG-DASH manifest: {0}")]
    DashManifestUnsupportedLive(String),

    /// Repeated refreshes returned an MPD with an older `MPD@publishTime` than
    /// the version already in use.
    #[error("Live MPEG-DASH manifest stayed stale after {attempts} refreshes")]
    DashManifestStale {
        /// Number of consecutive refreshes that returned an older MPD.
        attempts: u32,
    },

    /// A refreshed manifest has a gap after the previous segment timeline,
    /// so playback cannot continue seamlessly.
    #[error("Refreshed live MPEG-DASH manifest no longer reaches the segments before it: {0}")]
    DashManifestLiveGap(String),

    /// A representation's `SegmentBase` index could not be fetched or parsed
    /// when the manifest was loaded
    #[error(
        "MPEG-DASH SegmentBase index for representation {representation} failed to load: {reason}"
    )]
    DashManifestSegmentIndex {
        /// The representation ID
        representation: String,
        /// Why the index failed to load
        reason: String,
    },

    /// A MPEG-DASH URL could not be parsed or resolved against its BaseURL chain
    #[error("Cannot resolve MPEG-DASH URL {0}: {1}")]
    DashManifestUrl(String, #[source] url::ParseError),

    /// HTTP request for the MPEG-DASH manifest failed
    #[cfg(feature = "sync")]
    #[error("Fetching MPEG-DASH manifest failed: {0}")]
    DashManifestFetch(#[source] ureq::Error),

    /// HTTP request for the MPEG-DASH manifest failed
    #[cfg(feature = "async")]
    #[error("Requesting MPEG-DASH manifest failed: {0}")]
    DashManifestRequest(#[source] reqwest::Error),

    /// An MPEG-DASH representation did not include a media URL template
    #[error("MPEG-DASH representation is missing a media URL template")]
    DashManifestMissingMediaTemplate,

    /// A MPEG-DASH representation had a zero or out-of-range timescale
    #[error("MPEG-DASH representation has an invalid timescale")]
    DashManifestMissingTimescale,

    /// A byte range was not in `first-last` form with `first <= last`
    #[error("Invalid byte range: {0}")]
    InvalidByteRange(String),

    /// Media segments from a manifest or passed to
    /// [`Segments::timed`](crate::segment::Segments::timed) had a zero duration, or
    /// started before the previous segment ended
    #[error("Invalid segment timing: {0}")]
    InvalidSegmentTiming(String),

    /// A ranged segment's body did not match the length of its range
    #[error("Segment {segment} returned {actual} bytes, expected {expected}")]
    SegmentLength {
        /// The segment URL and range
        segment: String,
        /// Bytes covered by the range
        expected: u64,
        /// Bytes received
        actual: u64,
    },

    /// A reader, source or transport could not be set up, or cannot do what
    /// was asked of it
    #[error("Initialization error: {0}")]
    Initialization(String),

    /// IO error while reading a media segment body
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP request for a segment failed
    #[cfg(feature = "sync")]
    #[error("Fetching segment failed: {0}")]
    SegmentFetch(#[from] ureq::Error),

    /// HTTP request for a segment failed
    #[cfg(feature = "async")]
    #[error("Segment {segment} request failed: {source}")]
    SegmentRequest {
        /// The segment URL and range
        segment: String,
        /// Why the request failed
        #[source]
        source: reqwest::Error,
    },

    /// A segment's MP4 boxes are truncated or inconsistent
    #[cfg(any(feature = "sync", feature = "async"))]
    #[error("Segment {segment} has malformed MP4 boxes: {reason}")]
    MalformedMedia {
        /// The segment URL and range
        segment: String,
        /// What is malformed
        reason: String,
    },

    /// A segment uses MP4 features a [`PacketStream`](crate::packet::PacketStream) does not read, such as
    /// several tracks or samples still encrypted
    #[cfg(any(feature = "sync", feature = "async"))]
    #[error("Segment {segment} uses unsupported MP4 features: {reason}")]
    UnsupportedMedia {
        /// The segment URL and range
        segment: String,
        /// What is unsupported
        reason: String,
    },

    /// A track cannot be turned into decoder input: its codec has no adapter, or the
    /// requested format cannot carry it
    #[cfg(any(feature = "sync", feature = "async"))]
    #[error("{codec} cannot be decoder input in {}: {reason}", format_name(.format))]
    UnsupportedDecoderInput {
        /// The codec, such as `H.264`, or the sample entry type of an unknown one
        codec: String,
        /// The format asked for or chosen, or `None` if none was chosen
        format: Option<crate::decoder::DecoderFormat>,
        /// What is unsupported
        reason: String,
    },

    /// A codec configuration or packet does not follow its codec's format
    #[cfg(any(feature = "sync", feature = "async"))]
    #[error("Malformed {codec} decoder input in {format}: {reason}")]
    MalformedDecoderInput {
        /// The codec, such as `H.264`, or the sample entry type of an unknown one
        codec: String,
        /// The format being produced
        format: crate::decoder::DecoderFormat,
        /// What is malformed
        reason: String,
    },

    /// Rodio could not initialize a decoder for the selected representation.
    #[cfg(feature = "rodio")]
    #[error("Audio decoder initialization failed: {0}")]
    RodioDecoder(#[from] ::rodio::decoder::DecoderError),

    /// The rodio adapter was asked to decode a protected representation
    /// without content keys.
    #[cfg(feature = "rodio")]
    #[error("Rodio playback of {0}-protected MPEG-DASH content needs content keys")]
    RodioProtectedContent(String),

    /// A content key or key ID was not 32 hexadecimal digits
    #[cfg(feature = "encryption")]
    #[error("Invalid content key or KID: {0}")]
    InvalidContentKey(String),

    /// Protected media needs a key for this KID (hexadecimal) that was not supplied
    #[cfg(feature = "encryption")]
    #[error("No content key for KID {0}")]
    MissingContentKey(String),

    /// A segment uses Common Encryption features this crate does not decrypt
    #[cfg(feature = "encryption")]
    #[error("Segment {segment} uses unsupported content protection: {reason}")]
    UnsupportedProtection {
        /// The segment URL and range
        segment: String,
        /// What is unsupported
        reason: String,
    },

    /// A segment's Common Encryption boxes are truncated or inconsistent
    #[cfg(feature = "encryption")]
    #[error("Segment {segment} has malformed content protection: {reason}")]
    MalformedProtection {
        /// The segment URL and range
        segment: String,
        /// What is malformed
        reason: String,
    },
}

/// A decoder-input error's format, for its message.
#[cfg(any(feature = "sync", feature = "async"))]
fn format_name(format: &Option<crate::decoder::DecoderFormat>) -> String {
    match format {
        Some(format) => format.to_string(),
        None => "any format".to_owned(),
    }
}
