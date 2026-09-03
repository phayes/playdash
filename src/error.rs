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
    #[error("MPEG-DASH manifest XML parsing failed: {0}")]
    DashManifestXml(#[from] quick_xml::Error),

    /// MPEG-DASH manifest did not contain any stream URLs
    #[error("MPEG-DASH manifest contains no stream URLs")]
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

    /// A TIDAL MPEG-DASH representation did not include a SegmentTimeline
    #[error("MPEG-DASH representation is missing a SegmentTimeline")]
    DashManifestMissingTimeline,

    /// A TIDAL MPEG-DASH representation did not include a media URL template
    #[error("MPEG-DASH representation is missing a media URL template")]
    DashManifestMissingMediaTemplate,

    /// A TIDAL MPEG-DASH SegmentTimeline entry was missing a duration
    #[error("MPEG-DASH SegmentTimeline entry is missing a duration")]
    DashManifestInvalidTimeline,

    /// A TIDAL MPEG-DASH representation did not include a timescale
    #[error("MPEG-DASH representation is missing a timescale")]
    DashManifestMissingTimescale,

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

    /// The rodio adapter was asked to decode a protected representation.
    #[cfg(feature = "rodio")]
    #[error("Rodio playback does not support {0}-protected MPEG-DASH content")]
    RodioProtectedContent(String),
}
