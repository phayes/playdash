//! The audio and video choices listed by a DASH manifest.

use super::Selected;
use serde::{Deserialize, Serialize};

/// One encoded audio, video, or text choice in a DASH manifest.
///
/// A manifest often contains several representations of the same content, such as
/// multiple video resolutions or audio bitrates. Use [`DashManifest::representations`]
/// to list them, then pass [`Self::id`] to
/// [`DashManifest::media_stream`](crate::DashManifest::media_stream). Fields that are
/// absent from the representation inherit their value from its adaptation set.
///
/// [`DashManifest`]: crate::DashManifest
/// [`DashManifest::representations`]: crate::DashManifest::representations
/// [`DashManifest::fragments`]: crate::DashManifest::fragments
///
/// # Examples
///
/// ```
/// use playdash::{ContentKind, DashManifest};
///
/// let manifest = DashManifest::new(r#"
///     <MPD mediaPresentationDuration="PT4S"><Period>
///       <AdaptationSet mimeType="video/mp4" frameRate="30000/1001">
///         <Representation id="720p" codecs="avc1.64001f" width="1280" height="720">
///           <SegmentTemplate initialization="https://media.example/v/init.mp4"
///                            media="https://media.example/v/$Number$.m4s" duration="2" />
///         </Representation>
///       </AdaptationSet>
///       <AdaptationSet mimeType="audio/mp4" lang="en">
///         <Representation id="audio" codecs="mp4a.40.2" audioSamplingRate="48000">
///           <SegmentTemplate initialization="https://media.example/a/init.mp4"
///                            media="https://media.example/a/$Number$.m4s" duration="2" />
///         </Representation>
///       </AdaptationSet>
///     </Period></MPD>
/// "#)?;
/// let video = manifest
///     .representations()
///     .find(|representation| representation.kind == ContentKind::Video)
///     .unwrap();
/// assert_eq!((video.width, video.height), (Some(1280), Some(720)));
/// assert!((video.frame_rate.unwrap().as_f64() - 29.97).abs() < 0.01);
///
/// let audio = manifest.representation("audio").unwrap();
/// assert_eq!(audio.kind, ContentKind::Audio);
/// assert_eq!(audio.lang, Some("en"));
/// assert_eq!(audio.audio_sampling_rate, Some(48_000));
/// # Ok::<(), playdash::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Representation<'a> {
    /// Manifest-defined identifier used to open this representation.
    /// Pass it to [`DashManifest::media_stream`](crate::DashManifest::media_stream) or a
    /// lower-level selection method.
    pub id: &'a str,

    /// Advertised average bitrate in bits per second, or `None` when not declared.
    /// This is the value used by `highest_bandwidth` and `lowest_bandwidth`.
    pub bandwidth: Option<u64>,

    /// DASH codec string, such as `avc1.64001f` or `mp4a.40.2`.
    /// A muxed representation lists multiple codecs separated by commas. `None` means
    /// neither the representation nor its adaptation set declares a codec.
    pub codecs: Option<&'a str>,

    /// MIME type inherited from the representation or adaptation set, such as
    /// `video/mp4`, or `None` when neither declares one.
    pub mime_type: Option<&'a str>,

    /// Audio, video, text, muxed, or unknown media inferred from manifest metadata.
    pub kind: ContentKind,

    /// Coded video width in pixels, or `None` for audio/text or undeclared dimensions.
    pub width: Option<u64>,

    /// Coded video height in pixels, or `None` for audio/text or undeclared dimensions.
    pub height: Option<u64>,

    /// Advertised video frame rate, or `None` when absent or invalid.
    pub frame_rate: Option<FrameRate>,

    /// Advertised audio sample rate in Hz.
    /// When the manifest gives a range, this is its minimum. `None` means absent or invalid.
    pub audio_sampling_rate: Option<u32>,

    /// Content language as an RFC 5646 tag such as `en` or `pt-BR`, if declared.
    pub lang: Option<&'a str>,

    /// Roles of the adaptation set in the DASH role scheme
    /// (`urn:mpeg:dash:role:2011`), such as `main`, `alternate` or
    /// `commentary`.
    pub roles: Vec<&'a str>,

    /// Stable index of the containing adaptation set in manifest order.
    /// Representations with the same value are alternative encodings of the same logical
    /// content, such as several resolutions of one video angle.
    pub adaptation_set: usize,
}

/// A representation's kind of media.
///
/// Serialized in lowercase, such as `"video"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    /// Video only.
    Video,
    /// Audio only.
    Audio,
    /// Subtitles or captions.
    Text,
    /// Video and audio in the same representation.
    Muxed,
    /// Unrecognized or absent. A player may guess the kind from the codecs or MIME.
    Unknown,
}

/// A video frame rate as an exact fraction of frames per second, such as
/// `30000/1001` for NTSC's 29.97.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRate {
    /// Number of frames in each [`Self::denominator`]-second interval.
    pub numerator: u64,
    /// Seconds taken by [`Self::numerator`] frames. Never zero for parsed manifests.
    pub denominator: u64,
}

impl FrameRate {
    /// Converts this exact fraction to approximate frames per second.
    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    /// Parses a DASH `@frameRate`: `numerator` or `numerator/denominator`.
    fn parse(value: &str) -> Option<Self> {
        let (numerator, denominator) = match value.trim().split_once('/') {
            Some((numerator, denominator)) => (numerator, denominator),
            None => (value.trim(), "1"),
        };
        let rate = Self {
            numerator: numerator.parse().ok()?,
            denominator: denominator.parse().ok()?,
        };
        (rate.denominator > 0).then_some(rate)
    }
}

/// DASH's scheme for `Role` descriptors.
const ROLE_SCHEME: &str = "urn:mpeg:dash:role:2011";

impl<'a> Selected<'a> {
    pub(super) fn summary(self) -> Representation<'a> {
        let (representation, adaptation) = (self.representation, self.adaptation);
        Representation {
            id: self.id(),
            bandwidth: representation.bandwidth,
            codecs: self.codecs(),
            mime_type: self.mime_type(),
            kind: self.kind(),
            width: representation.width.or(adaptation.width),
            height: representation.height.or(adaptation.height),
            frame_rate: representation
                .frameRate
                .as_deref()
                .or(adaptation.frameRate.as_deref())
                .and_then(FrameRate::parse),
            audio_sampling_rate: representation
                .audioSamplingRate
                .as_deref()
                .or(adaptation.audioSamplingRate.as_deref())
                .and_then(parse_sampling_rate),
            lang: representation
                .lang
                .as_deref()
                .or(adaptation.lang.as_deref()),
            roles: adaptation
                .Role
                .iter()
                .filter(|role| role.schemeIdUri.eq_ignore_ascii_case(ROLE_SCHEME))
                .filter_map(|role| role.value.as_deref())
                .collect(),
            adaptation_set: self.adaptation_index,
        }
    }

    /// The kind of media: muxed if the codecs name both video and audio,
    /// else `@contentType`, else the codecs, else the MIME type.
    fn kind(self) -> ContentKind {
        let codec_kinds: Vec<_> = self
            .codecs()
            .unwrap_or_default()
            .split(',')
            .filter_map(codec_kind)
            .collect();
        if codec_kinds.contains(&ContentKind::Video) && codec_kinds.contains(&ContentKind::Audio) {
            return ContentKind::Muxed;
        }

        let content_type = self
            .representation
            .contentType
            .as_deref()
            .or(self.adaptation.contentType.as_deref());
        let declared = match content_type {
            Some("video") => Some(ContentKind::Video),
            Some("audio") => Some(ContentKind::Audio),
            Some("text") => Some(ContentKind::Text),
            _ => None,
        };
        declared
            .or(codec_kinds.first().copied())
            .or_else(|| self.mime_type().and_then(mime_kind))
            .unwrap_or(ContentKind::Unknown)
    }
}

/// The kind of media a codec in a DASH `@codecs` list encodes, by its
/// leading four-character code.
fn codec_kind(codec: &str) -> Option<ContentKind> {
    let fourcc = codec.trim().split('.').next()?.to_ascii_lowercase();
    match fourcc.as_str() {
        "avc1" | "avc2" | "avc3" | "avc4" | "hvc1" | "hev1" | "vvc1" | "vvi1" | "dvh1" | "dvhe"
        | "dva1" | "dvav" | "dav1" | "av01" | "vp08" | "vp09" | "vp8" | "vp9" | "mp4v" => {
            Some(ContentKind::Video)
        }
        "mp4a" | "ac-3" | "ec-3" | "ac-4" | "opus" | "flac" | "alac" | "mha1" | "mhm1" | "dtsc"
        | "dtse" | "dtsh" | "dtsl" | "dtsx" | "vorbis" | "mp3" => Some(ContentKind::Audio),
        "stpp" | "wvtt" | "tx3g" | "c608" => Some(ContentKind::Text),
        _ => None,
    }
}

/// The kind of media a MIME type names.
fn mime_kind(mime_type: &str) -> Option<ContentKind> {
    let mime_type = mime_type.trim().to_ascii_lowercase();
    if mime_type.starts_with("video/") {
        Some(ContentKind::Video)
    } else if mime_type.starts_with("audio/") {
        Some(ContentKind::Audio)
    } else if mime_type.starts_with("text/") || mime_type == "application/ttml+xml" {
        Some(ContentKind::Text)
    } else {
        None
    }
}

/// Parses a DASH `@audioSamplingRate`: one rate in Hz, or a minimum and a
/// maximum separated by whitespace, of which this is the minimum.
fn parse_sampling_rate(value: &str) -> Option<u32> {
    value.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DashManifest;

    fn manifest(adaptations: &str) -> DashManifest {
        DashManifest::new(format!(
            r#"<MPD mediaPresentationDuration="PT2S"><Period>{adaptations}</Period></MPD>"#
        ))
        .unwrap()
    }

    fn kind(adaptation: &str, representation: &str) -> ContentKind {
        let xml = format!(
            r#"<AdaptationSet {adaptation}><Representation id="r" {representation}/></AdaptationSet>"#
        );
        manifest(&xml).representation("r").unwrap().kind
    }

    #[test]
    fn kind_reads_codecs_content_type_and_mime_type() {
        assert_eq!(
            kind(
                r#"mimeType="video/mp4""#,
                r#"codecs="avc1.64001f,mp4a.40.2""#
            ),
            ContentKind::Muxed
        );
        // contentType wins over a MIME type that says otherwise.
        assert_eq!(
            kind(r#"contentType="audio" mimeType="video/mp4""#, ""),
            ContentKind::Audio
        );
        assert_eq!(
            kind(r#"mimeType="application/mp4""#, r#"codecs="stpp""#),
            ContentKind::Text
        );
        assert_eq!(
            kind(r#"mimeType="video/mp4""#, r#"codecs="hvc1.1.6.L93.B0""#),
            ContentKind::Video
        );
        assert_eq!(
            kind(r#"mimeType="audio/mp4""#, r#"codecs="unknown""#),
            ContentKind::Audio
        );
        assert_eq!(
            kind(r#"mimeType="application/octet-stream""#, ""),
            ContentKind::Unknown
        );
    }

    #[test]
    fn rates_and_sizes_inherit_from_the_adaptation_set() {
        let manifest = manifest(
            r#"<AdaptationSet mimeType="video/mp4" width="1920" height="1080" frameRate="25" lang="fr">
                 <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
                 <Role schemeIdUri="urn:example:other" value="ignored"/>
                 <Representation id="hd"/>
                 <Representation id="sd" width="640" height="360" frameRate="30000/1001"/>
               </AdaptationSet>
               <AdaptationSet mimeType="audio/mp4" audioSamplingRate="44100 48000">
                 <Representation id="aac"/>
                 <Representation id="bad" audioSamplingRate="fast"/>
               </AdaptationSet>"#,
        );
        let hd = manifest.representation("hd").unwrap();
        assert_eq!((hd.width, hd.height), (Some(1920), Some(1080)));
        assert_eq!(
            hd.frame_rate,
            Some(FrameRate {
                numerator: 25,
                denominator: 1
            })
        );
        assert_eq!(hd.lang, Some("fr"));
        assert_eq!(hd.roles, ["main"]);
        assert_eq!(hd.adaptation_set, 0);

        let sd = manifest.representation("sd").unwrap();
        assert_eq!((sd.width, sd.height), (Some(640), Some(360)));
        assert_eq!(sd.frame_rate.unwrap().denominator, 1001);
        assert_eq!(sd.adaptation_set, 0);

        let aac = manifest.representation("aac").unwrap();
        assert_eq!(aac.audio_sampling_rate, Some(44_100));
        assert_eq!(aac.adaptation_set, 1);
        assert!(aac.roles.is_empty());
        assert_eq!(
            manifest.representation("bad").unwrap().audio_sampling_rate,
            None
        );
    }

    #[test]
    fn frame_rate_parses_dash_forms() {
        assert_eq!(FrameRate::parse("24").map(FrameRate::as_f64), Some(24.0));
        assert_eq!(
            FrameRate::parse("30000/1001"),
            Some(FrameRate {
                numerator: 30_000,
                denominator: 1001
            })
        );
        for invalid in ["", "25/0", "29.97", "a/b", "/2"] {
            assert_eq!(FrameRate::parse(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn serializes_kind_in_lowercase() {
        let manifest = manifest(
            r#"<AdaptationSet mimeType="audio/mp4"><Representation id="a"/></AdaptationSet>"#,
        );
        let representation = manifest.representation("a").unwrap();
        let json = serde_json::to_value(&representation).unwrap();
        assert_eq!(json["kind"], "audio");
    }
}
