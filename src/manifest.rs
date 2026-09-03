//! Parsed TIDAL MPEG-DASH manifests.

use crate::error::Error;
use crate::stream::MediaTimeline;
use crate::stream::MpegStreamReader;
use base64::{Engine, engine::general_purpose::STANDARD};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use serde::{Deserialize, Serialize};

/// Decoded streaming information from an MPEG-DASH manifest.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct DashManifest {
    /// Original MPEG-DASH manifest XML.
    pub dash_xml: String,
    /// Representations in MPEG-DASH document order.
    pub representations: Vec<DashRepresentation>,
}

/// A playable representation within a TIDAL MPEG-DASH manifest.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct DashRepresentation {
    /// TIDAL audio format identifier from the DASH representation.
    pub id: String,
    /// MIME type inherited from the DASH adaptation set.
    pub mime_type: String,
    /// Codec declared by the DASH representation.
    pub codecs: String,
    /// Common-encryption scheme inherited from the adaptation set, such as `cenc` or `cbcs`.
    pub protection_scheme: Option<String>,
    /// URLs and URL templates present in the manifest.
    pub urls: Vec<String>,
    /// Representation bitrate in bits per second.
    pub bitrate: Option<u32>,
    /// Initialization segment URL or template.
    pub initialization_url: Option<String>,
    /// Media segment URL template.
    pub media_url_template: Option<String>,
    /// Units per second used by the segment timeline.
    pub timescale: Option<u32>,
    /// Duration of each segment in timescale units.
    pub duration: Option<u32>,
    /// Number assigned to the first media segment.
    pub start_number: Option<u32>,
    /// Timeline entries that enumerate media segments.
    pub timeline: Vec<DashSegment>,
}

/// One `<S>` entry from a DASH `SegmentTimeline`.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct DashSegment {
    /// Segment duration in timescale units.
    pub duration: u64,
    /// Additional repeats of this duration. The segment is emitted `repeat + 1` times.
    pub repeat: u64,
}

fn unescape_attribute(attribute: &quick_xml::events::attributes::Attribute<'_>) -> String {
    attribute
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| attribute.value.as_ref().to_owned())
}

fn decode_data_url(data_url: &str) -> Result<String, Error> {
    let (metadata, encoded) = data_url
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
        .ok_or(Error::InvalidDataUrl)?;

    if !metadata
        .split(';')
        .skip(1)
        .any(|parameter| parameter.eq_ignore_ascii_case("base64"))
    {
        return Err(Error::InvalidDataUrl);
    }

    let bytes = STANDARD.decode(encoded)?;
    Ok(String::from_utf8(bytes)?)
}

impl DashRepresentation {
    fn parse(
        element: &BytesStart<'_>,
        mime_type: &str,
        protection_scheme: Option<&str>,
    ) -> Result<Self, Error> {
        let mut representation = Self {
            mime_type: mime_type.to_owned(),
            protection_scheme: protection_scheme.map(str::to_owned),
            ..Default::default()
        };

        for attribute in element.attributes().flatten() {
            match attribute.key.as_ref() {
                "id" => representation.id = attribute.value.as_ref().to_owned(),
                "codecs" => representation.codecs = attribute.value.as_ref().to_owned(),
                "bandwidth" => representation.bitrate = attribute.value.as_ref().parse().ok(),
                _ => {}
            }
        }

        if representation.id.is_empty() {
            return Err(Error::DashManifestMissingRepresentationId);
        }

        Ok(representation)
    }

    fn parse_segment_template(&mut self, element: &BytesStart<'_>) {
        for attribute in element.attributes().flatten() {
            match attribute.key.as_ref() {
                "initialization" => self.initialization_url = Some(unescape_attribute(&attribute)),
                "media" => self.media_url_template = Some(unescape_attribute(&attribute)),
                "timescale" => self.timescale = unescape_attribute(&attribute).parse().ok(),
                "duration" => self.duration = unescape_attribute(&attribute).parse().ok(),
                "startNumber" => self.start_number = unescape_attribute(&attribute).parse().ok(),
                _ => {}
            }
        }
    }

    fn parse_timeline_s(&mut self, element: &BytesStart<'_>) -> Result<(), Error> {
        let mut duration = None;
        let mut repeat = 0;
        for attribute in element.attributes().flatten() {
            match attribute.key.as_ref() {
                "d" => {
                    duration = unescape_attribute(&attribute).parse().ok();
                }
                "r" => {
                    repeat = unescape_attribute(&attribute).parse().unwrap_or(0);
                }
                _ => {}
            }
        }
        self.timeline.push(DashSegment {
            duration: duration.ok_or(Error::DashManifestInvalidTimeline)?,
            repeat,
        });
        Ok(())
    }

    /// Initialization URL followed by media segment URLs in timeline order.
    pub fn fragment_urls(&self) -> Result<Vec<String>, Error> {
        let initialization = self
            .initialization_url
            .clone()
            .ok_or(Error::DashManifestMissingUrls)?;
        let template = self
            .media_url_template
            .as_ref()
            .ok_or(Error::DashManifestMissingMediaTemplate)?;
        if self.timeline.is_empty() {
            return Err(Error::DashManifestMissingTimeline);
        }

        let mut urls = vec![initialization];
        let mut number = u64::from(self.start_number.unwrap_or(1));
        for entry in &self.timeline {
            for _ in 0..=entry.repeat {
                urls.push(template.replace("$Number$", &number.to_string()));
                number += 1;
            }
        }
        Ok(urls)
    }

    /// Media-segment durations in timeline order, with the DASH timescale.
    pub fn media_timeline(&self) -> Result<MediaTimeline, Error> {
        let timescale = self.timescale.ok_or(Error::DashManifestMissingTimescale)?;
        if self.timeline.is_empty() {
            return Err(Error::DashManifestMissingTimeline);
        }

        let mut media_durations = Vec::new();
        for entry in &self.timeline {
            for _ in 0..=entry.repeat {
                media_durations.push(entry.duration);
            }
        }

        Ok(MediaTimeline {
            timescale,
            media_durations,
        })
    }

    fn finish(mut self) -> Result<Self, Error> {
        if let Some(url) = &self.initialization_url {
            self.urls.push(url.clone());
        }
        if let Some(url) = &self.media_url_template {
            self.urls.push(url.clone());
        }
        if self.urls.is_empty() {
            return Err(Error::DashManifestMissingUrls);
        }

        Ok(self)
    }
}

impl DashManifest {
    /// Parses an MPEG-DASH manifest into streaming information.
    pub fn new(dash_xml: String) -> Result<Self, Error> {
        Self::parse_dash_manifest(dash_xml)
    }

    /// Decodes a base64 `data:` URL and parses the embedded MPEG-DASH XML.
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        let dash_xml = decode_data_url(data_url)?;
        Self::new(dash_xml)
    }

    /// Starts a progressive in-memory reader over fragmented MPEG-DASH bytes.
    ///
    /// `id` is matched case-insensitively against a representation's full ID,
    /// then against the format token before the first comma. The first match
    /// in document order wins.
    ///
    /// The initialization fragment is fetched before returning (and establishes
    /// the GET connection). Remaining media fragments download on a GET worker
    /// (from the playhead forward, then earlier holes). A second connection HEADs
    /// remaining URLs to fill fragment sizes. [`MpegStreamReader::seek_bytes`]
    /// may also HEAD to map offsets without downloading skipped bodies; if HEAD
    /// is unusable, it falls back to waiting on GETs. Call
    /// [`MpegStreamReader::set_symphonia_compat`] before handing the reader to
    /// a demuxer that seeks to EOF for the file length. Media timestamps use
    /// [`MpegStreamReader::seek_time_coarse`], which lands at the beginning of
    /// the containing fMP4 fragment and returns a
    /// [`crate::stream::Position`].
    pub fn stream(&self, id: impl AsRef<str>) -> Result<MpegStreamReader, Error> {
        let id = id.as_ref();
        let representation = self
            .representation(id)
            .ok_or_else(|| Error::DashManifestMissingRepresentation(id.to_owned()))?;
        let urls = representation.fragment_urls()?;
        let timeline = representation.media_timeline().ok();
        MpegStreamReader::new(urls, timeline)
    }

    /// Finds a representation by ID.
    ///
    /// The first pass matches `id` against the full representation ID. If that
    /// misses, a second pass matches the format token before the first comma.
    /// Both comparisons are case-insensitive. The first match in document order
    /// wins.
    pub fn representation(&self, id: impl AsRef<str>) -> Option<&DashRepresentation> {
        let id = id.as_ref();
        self.representations
            .iter()
            .find(|representation| representation.id.eq_ignore_ascii_case(id))
            .or_else(|| {
                self.representations.iter().find(|representation| {
                    representation_token(&representation.id).eq_ignore_ascii_case(id)
                })
            })
    }

    /// Parses MPEG-DASH XML and extracts its representation and segment details.
    pub fn parse_dash_manifest(dash_xml: String) -> Result<Self, Error> {
        let mut reader = Reader::from_str(&dash_xml);
        reader.config_mut().trim_text(true);

        let mut adaptation_mime_type = String::new();
        let mut adaptation_protection_scheme = None;
        let mut representations = Vec::new();
        let mut current_representation = None;
        let mut buffer = Vec::new();

        loop {
            match reader.read_event_into(&mut buffer)? {
                Event::Start(element) => match element.name().as_ref() {
                    "AdaptationSet" => {
                        adaptation_protection_scheme = None;
                        for attribute in element.attributes().flatten() {
                            if attribute.key.as_ref() == "mimeType" {
                                adaptation_mime_type = attribute.value.as_ref().to_owned();
                            }
                        }
                    }
                    "Representation" => {
                        current_representation = Some(DashRepresentation::parse(
                            &element,
                            &adaptation_mime_type,
                            adaptation_protection_scheme.as_deref(),
                        )?);
                    }
                    "ContentProtection" => {
                        if let Some(scheme) = common_encryption_scheme(&element) {
                            if let Some(representation) = &mut current_representation {
                                representation.protection_scheme = Some(scheme);
                            } else {
                                adaptation_protection_scheme = Some(scheme);
                            }
                        }
                    }
                    "SegmentTemplate" => {
                        if let Some(representation) = &mut current_representation {
                            representation.parse_segment_template(&element);
                        }
                    }
                    "S" => {
                        if let Some(representation) = &mut current_representation {
                            representation.parse_timeline_s(&element)?;
                        }
                    }
                    "BaseURL" => {
                        if let Event::Text(text) = reader.read_event_into(&mut buffer)? {
                            let url = quick_xml::escape::unescape(text.as_ref())
                                .map(|value| value.into_owned())
                                .unwrap_or_else(|_| text.as_ref().to_owned());
                            if !url.is_empty()
                                && let Some(representation) = &mut current_representation
                            {
                                representation.urls.push(url);
                            }
                        }
                    }
                    _ => {}
                },
                Event::Empty(element) => match element.name().as_ref() {
                    "ContentProtection" => {
                        if let Some(scheme) = common_encryption_scheme(&element) {
                            if let Some(representation) = &mut current_representation {
                                representation.protection_scheme = Some(scheme);
                            } else {
                                adaptation_protection_scheme = Some(scheme);
                            }
                        }
                    }
                    "SegmentTemplate" => {
                        if let Some(representation) = &mut current_representation {
                            representation.parse_segment_template(&element);
                        }
                    }
                    "S" => {
                        if let Some(representation) = &mut current_representation {
                            representation.parse_timeline_s(&element)?;
                        }
                    }
                    _ => {}
                },
                Event::End(element) if element.name().as_ref() == "Representation" => {
                    let representation = current_representation
                        .take()
                        .ok_or(Error::DashManifestMissingRepresentationId)?
                        .finish()?;
                    representations.push(representation);
                }
                Event::Eof => break,
                _ => {}
            }
            buffer.clear();
        }

        if representations.is_empty() {
            return Err(Error::DashManifestMissingRepresentations);
        }

        Ok(Self {
            dash_xml,
            representations,
        })
    }
}

fn representation_token(id: &str) -> &str {
    id.split(',').next().unwrap_or(id)
}

fn common_encryption_scheme(element: &BytesStart<'_>) -> Option<String> {
    let mut scheme_id_uri = None;
    let mut value = None;

    for attribute in element.attributes().flatten() {
        match attribute.key.as_ref() {
            "schemeIdUri" => scheme_id_uri = Some(unescape_attribute(&attribute)),
            "value" => value = Some(unescape_attribute(&attribute)),
            _ => {}
        }
    }

    scheme_id_uri
        .is_some_and(|uri| uri.eq_ignore_ascii_case("urn:mpeg:dash:mp4protection:2011"))
        .then(|| value.unwrap_or_else(|| "common encryption".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dash_manifest_fields() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC,44100,16" codecs="flac" bandwidth="1411200">
                            <BaseURL>https://media.example/audio/</BaseURL>
                            <SegmentTemplate
                                initialization="init.mp4"
                                media="segment_$Number$.m4s"
                                timescale="48000"
                                duration="192000"
                                startNumber="1"
                            />
                        </Representation>
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="320000">
                            <SegmentTemplate
                                initialization="aac-init.mp4"
                                media="aac-segment_$Number$.m4s"
                                timescale="44100"
                                startNumber="1"
                            />
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let stream = DashManifest::new(xml.to_owned()).unwrap();

        assert_eq!(stream.dash_xml, xml);
        assert_eq!(stream.representations.len(), 2);

        let flac = stream.representation("FLAC").unwrap();
        assert_eq!(flac.id, "FLAC,44100,16");
        assert_eq!(flac.mime_type, "audio/mp4");
        assert_eq!(flac.codecs, "flac");
        assert_eq!(flac.bitrate, Some(1_411_200));
        assert_eq!(flac.initialization_url.as_deref(), Some("init.mp4"));
        assert_eq!(
            flac.media_url_template.as_deref(),
            Some("segment_$Number$.m4s")
        );
        assert_eq!(flac.timescale, Some(48_000));
        assert_eq!(flac.duration, Some(192_000));
        assert_eq!(flac.start_number, Some(1));
        assert_eq!(
            flac.urls,
            [
                "https://media.example/audio/",
                "init.mp4",
                "segment_$Number$.m4s"
            ]
        );

        let aac = stream.representation("AACLC").unwrap();
        assert_eq!(aac.id, "AACLC");
        assert_eq!(aac.codecs, "mp4a.40.2");
        assert_eq!(aac.bitrate, Some(320_000));
        assert_eq!(aac.timescale, Some(44_100));
        assert!(aac.timeline.is_empty());
        assert!(matches!(
            aac.fragment_urls(),
            Err(Error::DashManifestMissingTimeline)
        ));
        assert!(matches!(
            aac.media_timeline(),
            Err(Error::DashManifestMissingTimeline)
        ));
    }

    #[test]
    fn parses_tidal_segment_timeline_and_unescapes_urls() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="321744">
                            <SegmentTemplate
                                timescale="44100"
                                initialization="https://cdn.example/0.mp4?token=abc&amp;info=init"
                                media="https://cdn.example/$Number$.mp4?token=abc&amp;info=media"
                                startNumber="1"
                            >
                                <SegmentTimeline>
                                    <S d="176128" r="2"/>
                                    <S d="108735"/>
                                </SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let stream = DashManifest::new(xml.to_owned()).unwrap();
        let aac = stream.representation("AACLC").unwrap();
        assert_eq!(
            aac.initialization_url.as_deref(),
            Some("https://cdn.example/0.mp4?token=abc&info=init")
        );
        assert_eq!(
            aac.media_url_template.as_deref(),
            Some("https://cdn.example/$Number$.mp4?token=abc&info=media")
        );
        assert_eq!(
            aac.timeline,
            [
                DashSegment {
                    duration: 176_128,
                    repeat: 2,
                },
                DashSegment {
                    duration: 108_735,
                    repeat: 0,
                }
            ]
        );
        assert_eq!(
            aac.fragment_urls().unwrap(),
            [
                "https://cdn.example/0.mp4?token=abc&info=init",
                "https://cdn.example/1.mp4?token=abc&info=media",
                "https://cdn.example/2.mp4?token=abc&info=media",
                "https://cdn.example/3.mp4?token=abc&info=media",
                "https://cdn.example/4.mp4?token=abc&info=media",
            ]
        );
        assert_eq!(
            aac.media_timeline().unwrap(),
            MediaTimeline {
                timescale: 44_100,
                media_durations: vec![176_128, 176_128, 176_128, 108_735],
            }
        );
    }

    #[test]
    fn inherits_common_encryption_scheme_from_adaptation_set() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <ContentProtection
                            schemeIdUri="urn:mpeg:dash:mp4protection:2011"
                            value="cbcs"
                        />
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                                <SegmentTimeline><S d="1000"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        let manifest = DashManifest::new(xml.to_owned()).unwrap();
        assert_eq!(
            manifest
                .representation("FLAC")
                .unwrap()
                .protection_scheme
                .as_deref(),
            Some("cbcs")
        );
    }

    #[test]
    fn stream_rejects_unknown_format() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <SegmentTemplate
                                initialization="init.mp4"
                                media="segment_$Number$.m4s"
                                startNumber="1"
                            >
                                <SegmentTimeline>
                                    <S d="1000"/>
                                </SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let stream = DashManifest::new(xml.to_owned()).unwrap();
        let error = stream.stream("FLAC").unwrap_err();
        assert!(matches!(
            error,
            Error::DashManifestMissingRepresentation(id) if id == "FLAC"
        ));
    }

    #[test]
    fn representation_prefers_full_id_then_format_token() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC_HIRES,48000,24" codecs="flac" bandwidth="2304000">
                            <SegmentTemplate initialization="hires-init.mp4" media="hires_$Number$.m4s"/>
                        </Representation>
                        <Representation id="FLAC,44100,16" codecs="flac" bandwidth="1411200">
                            <SegmentTemplate initialization="init.mp4" media="segment_$Number$.m4s"/>
                        </Representation>
                        <Representation id="AACLC" codecs="mp4a.40.2" bandwidth="320000">
                            <SegmentTemplate initialization="aac-init.mp4" media="aac_$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let stream = DashManifest::new(xml.to_owned()).unwrap();

        assert_eq!(
            stream.representation("FLAC_HIRES,48000,24").unwrap().id,
            "FLAC_HIRES,48000,24"
        );
        assert_eq!(
            stream.representation("flac,44100,16").unwrap().id,
            "FLAC,44100,16"
        );
        assert_eq!(stream.representation("FLAC").unwrap().id, "FLAC,44100,16");
        assert_eq!(
            stream.representation("flac_hires").unwrap().id,
            "FLAC_HIRES,48000,24"
        );
        assert_eq!(stream.representation("aaclc").unwrap().id, "AACLC");
        assert!(stream.representation("MP3").is_none());
    }

    #[test]
    fn rejects_dash_manifest_without_representations() {
        assert!(matches!(
            DashManifest::new("<MPD/>".to_owned()),
            Err(Error::DashManifestMissingRepresentations)
        ));
    }

    #[test]
    fn rejects_representation_without_id() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation codecs="flac">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;

        assert!(matches!(
            DashManifest::new(xml.to_owned()),
            Err(Error::DashManifestMissingRepresentationId)
        ));
    }

    #[test]
    fn new_from_data_url_decodes_and_parses() {
        let xml = r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="AACLC" codecs="mp4a.40.2">
                            <SegmentTemplate initialization="init.mp4" media="$Number$.m4s"/>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
        "#;
        let encoded = STANDARD.encode(xml);
        let manifest =
            DashManifest::new_from_data_url(&format!("data:application/dash+xml;base64,{encoded}"))
                .unwrap();
        assert!(manifest.representation("AACLC").is_some());
    }

    #[test]
    fn new_from_data_url_rejects_non_data_url() {
        assert!(matches!(
            DashManifest::new_from_data_url("https://example.com/manifest.mpd"),
            Err(Error::InvalidDataUrl)
        ));
    }
}
