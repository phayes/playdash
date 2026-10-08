//! Errors returned by DASH parsing and fragment streaming.

/// Errors that can occur when parsing a DASH manifest or streaming fragments.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Manifest was not a base64-encoded `data:` URL
    #[error("Invalid DASH manifest data URL")]
    InvalidDataUrl,

    /// Base64-encoded data URL could not be decoded
    #[error("DASH manifest base64 decoding failed: {0}")]
    DataUrlBase64(#[from] base64::DecodeError),

    /// Decoded data URL payload was not valid UTF-8
    #[error("DASH manifest UTF-8 decoding failed: {0}")]
    DataUrlUtf8(#[from] std::string::FromUtf8Error),

    /// MPEG-DASH manifest XML could not be parsed
    #[error("MPEG-DASH manifest parsing failed: {0}")]
    DashManifestParse(#[from] dash_mpd_core::DashMpdError),

    /// A TIDAL MPEG-DASH representation did not include an initialization URL
    #[error("MPEG-DASH representation is missing an initialization URL")]
    DashManifestMissingUrls,

    /// MPEG-DASH manifest did not contain any representations
    #[error("MPEG-DASH manifest contains no representations")]
    DashManifestMissingRepresentations,

    /// A TIDAL MPEG-DASH representation did not have an ID
    #[error("MPEG-DASH representation is missing its TIDAL format ID")]
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
    #[error("Fetching MPEG-DASH manifest failed: {0}")]
    DashManifestFetch(#[source] ureq::Error),

    /// A TIDAL MPEG-DASH representation did not include a media URL template
    #[error("MPEG-DASH representation is missing a media URL template")]
    DashManifestMissingMediaTemplate,

    /// A MPEG-DASH representation had a zero or out-of-range timescale
    #[error("MPEG-DASH representation has an invalid timescale")]
    DashManifestMissingTimescale,

    /// A byte range was not in `first-last` form with `first <= last`
    #[error("Invalid byte range: {0}")]
    InvalidByteRange(String),

    /// A ranged fragment's body did not match the length of its range
    #[error("Fragment {fragment} returned {actual} bytes, expected {expected}")]
    FragmentLength {
        /// The fragment URL and range
        fragment: String,
        /// Bytes covered by the range
        expected: u64,
        /// Bytes received
        actual: u64,
    },

    /// Failed to initialize audio stream
    #[error("Stream initialization error: {0}")]
    StreamInitializationError(String),

    /// IO error while reading a media fragment body
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP request for a media fragment failed
    #[error("Stream HTTP request failed: {0}")]
    StreamHttp(#[from] ureq::Error),

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

    /// A fragment uses Common Encryption features this crate does not decrypt
    #[cfg(feature = "encryption")]
    #[error("Fragment {fragment} uses unsupported content protection: {reason}")]
    UnsupportedProtection {
        /// The fragment URL and range
        fragment: String,
        /// What is unsupported
        reason: String,
    },

    /// A fragment's Common Encryption boxes are truncated or inconsistent
    #[cfg(feature = "encryption")]
    #[error("Fragment {fragment} has malformed content protection: {reason}")]
    MalformedProtection {
        /// The fragment URL and range
        fragment: String,
        /// What is malformed
        reason: String,
    },
}
