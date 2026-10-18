use super::*;
use crate::segment::timed_media_for_test;

/// The init URL and the media URLs of a representation without byte ranges.
fn urls(manifest: &DashManifest, id: &str) -> (String, Vec<String>) {
    let url = |location: &SegmentLocation| {
        assert_eq!(location.range, None);
        location.url.clone()
    };
    let segments = manifest.segments(id).unwrap();
    let media = segments.media().map(|location| url(&location)).collect();
    (url(segments.init()), media)
}

/// The locations of the init segment and the media segments of `segments`.
fn parts(segments: &Segments) -> (SegmentLocation, Vec<SegmentLocation>) {
    (segments.init().clone(), segments.media().collect())
}

/// The timescale of a representation's media segments.
fn timescale(manifest: &DashManifest, id: &str) -> u32 {
    manifest
        .segments(id)
        .unwrap()
        .timing()
        .unwrap()
        .timescale
        .get()
}

/// The duration of each of a representation's media segments, in its ticks.
fn durations(manifest: &DashManifest, id: &str) -> Vec<u64> {
    let segments = manifest.segments(id).unwrap();
    let media = timed_media_for_test(&segments).unwrap();
    media.iter().map(|segment| segment.duration).collect()
}

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

    let manifest = DashManifest::new(xml).unwrap();
    assert_eq!(manifest.representations().count(), 2);

    let flac = manifest.representation("FLAC").unwrap();
    assert_eq!(flac.id, "FLAC,44100,16");
    assert_eq!(flac.codecs, Some("flac"));
    assert_eq!(flac.bandwidth, Some(1_411_200));
    assert_eq!(flac.mime_type, Some("audio/mp4"));

    let core = manifest.find("FLAC").unwrap().representation;
    assert_eq!(core.BaseURL[0].base, "https://media.example/audio/");
    let template = core.SegmentTemplate.as_ref().unwrap();
    assert_eq!(template.initialization.as_deref(), Some("init.mp4"));
    assert_eq!(template.media.as_deref(), Some("segment_$Number$.m4s"));
    assert_eq!(template.timescale, Some(48_000));
    assert_eq!(template.duration, Some(192_000.0));
    assert_eq!(template.startNumber, Some(1));

    let aac = manifest.representation("AACLC").unwrap();
    assert_eq!(aac.codecs, Some("mp4a.40.2"));
    assert_eq!(aac.bandwidth, Some(320_000));
    assert!(matches!(
        manifest.segments("FLAC"),
        Err(Error::DashManifestMissingDuration)
    ));
    assert!(matches!(
        manifest.segments("AACLC"),
        Err(Error::DashManifestMissingTimeline)
    ));
}

#[test]
fn parses_segment_timeline_and_unescapes_urls() {
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

    let manifest = DashManifest::new(xml).unwrap();
    let (init, media) = urls(&manifest, "AACLC");
    assert_eq!(init, "https://cdn.example/0.mp4?token=abc&info=init");
    assert_eq!(
        media,
        [
            "https://cdn.example/1.mp4?token=abc&info=media",
            "https://cdn.example/2.mp4?token=abc&info=media",
            "https://cdn.example/3.mp4?token=abc&info=media",
            "https://cdn.example/4.mp4?token=abc&info=media",
        ]
    );
    assert_eq!(timescale(&manifest, "AACLC"), 44_100);
    assert_eq!(
        durations(&manifest, "AACLC"),
        [176_128, 176_128, 176_128, 108_735]
    );
    assert_eq!(
        manifest.segments("AACLC").unwrap().span(0).unwrap().start,
        Duration::ZERO
    );
}

#[test]
fn inherits_segment_template_from_adaptation_set() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <SegmentTemplate timescale="1000" initialization="init.mp4" media="$Number$.mp4">
                        <SegmentTimeline><S d="2000" r="1"/></SegmentTimeline>
                    </SegmentTemplate>
                    <Representation id="FLAC" codecs="flac">
                        <SegmentTemplate startNumber="5"/>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;

    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url("https://cdn.example/track/manifest.mpd")
        .unwrap();
    let (init, media) = urls(&manifest, "FLAC");
    assert_eq!(init, "https://cdn.example/track/init.mp4");
    assert_eq!(
        media,
        [
            "https://cdn.example/track/5.mp4",
            "https://cdn.example/track/6.mp4",
        ]
    );
    assert_eq!(timescale(&manifest, "FLAC"), 1000);
    assert_eq!(durations(&manifest, "FLAC"), [2000, 2000]);
}

#[test]
fn segments_keep_their_media_time_and_present_from_the_offset() {
    let segments = |timing: &str| {
        DashManifest::new(format!(
            r#"<MPD mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="audio/mp4">
                <Representation id="FLAC">
                    <SegmentTemplate initialization="init" media="$Number$" {timing}
                </Representation>
            </AdaptationSet></Period></MPD>"#
        ))
        .unwrap()
        .with_base_url("https://cdn.example/manifest.mpd")
        .unwrap()
        .segments("FLAC")
        .unwrap()
    };
    let starts = |segments: &Segments| -> Vec<u64> {
        let media = timed_media_for_test(segments).unwrap();
        media.iter().map(|segment| segment.start).collect()
    };
    let millis = Duration::from_millis;

    // A timeline starting after the presentation time offset presents late.
    let late = segments(
        r#"timescale="1000" presentationTimeOffset="500">
            <SegmentTimeline><S t="700" d="2000" r="1"/></SegmentTimeline>
        </SegmentTemplate>"#,
    );
    assert_eq!(late.timing().unwrap().presentation_time_offset, 500);
    assert_eq!(starts(&late), [700, 2700]);
    assert_eq!(late.span(0), Some(millis(200)..millis(2_200)));
    // The Period ends at media time 4500, cutting the second segment short.
    assert_eq!(late.span(1), Some(millis(2_200)..millis(4_000)));

    // Fixed-duration segments start at the offset, so present from zero.
    let fixed = segments(r#"timescale="1000" presentationTimeOffset="500" duration="2000"/>"#);
    assert_eq!(starts(&fixed), [500, 2500]);
    assert_eq!(fixed.span(0), Some(Duration::ZERO..millis(2_000)));

    // Without either, media time starts at zero.
    let plain = segments(
        r#"timescale="1000"><SegmentTimeline><S d="2000"/></SegmentTimeline></SegmentTemplate>"#,
    );
    assert_eq!(starts(&plain), [0]);
    assert_eq!(plain.span(0), Some(Duration::ZERO..millis(2_000)));
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

    let manifest = DashManifest::new(xml).unwrap();
    assert_eq!(
        manifest.protection_scheme("FLAC").unwrap().as_deref(),
        Some("cbcs")
    );
}

#[test]
#[cfg(feature = "sync")]
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
    let manifest = DashManifest::new(xml).unwrap();
    let error = manifest.segment_stream("FLAC").unwrap_err();
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
    let manifest = DashManifest::new(xml).unwrap();
    let id_of = |id: &str| manifest.representation(id).unwrap().id;

    assert_eq!(id_of("FLAC_HIRES,48000,24"), "FLAC_HIRES,48000,24");
    assert_eq!(id_of("flac,44100,16"), "FLAC,44100,16");
    assert_eq!(id_of("FLAC"), "FLAC,44100,16");
    assert_eq!(id_of("flac_hires"), "FLAC_HIRES,48000,24");
    assert_eq!(id_of("aaclc"), "AACLC");
    assert!(manifest.representation("MP3").is_none());
}

#[test]
fn picks_representations_by_kind_and_bandwidth() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet contentType="video" mimeType="video/mp4">
                    <Representation id="v-none" codecs="avc1.64001f"/>
                    <Representation id="v-mid" codecs="avc1.64001f" bandwidth="2000000"/>
                    <Representation id="v-high" codecs="avc1.64001f" bandwidth="5000000"/>
                    <Representation id="v-high-again" codecs="avc1.64001f" bandwidth="5000000"/>
                    <Representation id="v-low" codecs="avc1.64001f" bandwidth="500000"/>
                    <Representation id="v-low-again" codecs="avc1.64001f" bandwidth="500000"/>
                </AdaptationSet>
                <AdaptationSet mimeType="video/mp4">
                    <Representation id="muxed" codecs="avc1.64001f,mp4a.40.2" bandwidth="9000000"/>
                </AdaptationSet>
                <AdaptationSet contentType="audio" mimeType="audio/mp4">
                    <Representation id="a-none" codecs="mp4a.40.2"/>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml).unwrap();
    let ids = |kind| -> Vec<&str> {
        manifest
            .representations_of(kind)
            .map(|representation| representation.id)
            .collect()
    };

    // Exactly the kind asked for, in document order.
    assert_eq!(
        ids(ContentKind::Video),
        [
            "v-none",
            "v-mid",
            "v-high",
            "v-high-again",
            "v-low",
            "v-low-again"
        ]
    );
    assert_eq!(ids(ContentKind::Muxed), ["muxed"]);
    assert!(ids(ContentKind::Text).is_empty());

    // Among advertised bandwidths, the first of equals.
    let id = |representation: Option<Representation>| representation.map(|r| r.id.to_owned());
    assert_eq!(
        id(manifest.highest_bandwidth(ContentKind::Video)).as_deref(),
        Some("v-high")
    );
    assert_eq!(
        id(manifest.lowest_bandwidth(ContentKind::Video)).as_deref(),
        Some("v-low")
    );
    assert_eq!(
        id(manifest.highest_bandwidth(ContentKind::Muxed)).as_deref(),
        Some("muxed")
    );
    assert_eq!(id(manifest.highest_bandwidth(ContentKind::Audio)), None);
    assert_eq!(id(manifest.lowest_bandwidth(ContentKind::Audio)), None);
    assert_eq!(id(manifest.lowest_bandwidth(ContentKind::Text)), None);
}

#[test]
fn rejects_dash_manifest_without_representations() {
    assert!(matches!(
        DashManifest::new("<MPD/>"),
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
        DashManifest::new(xml),
        Err(Error::DashManifestMissingRepresentationId)
    ));
}

#[test]
fn rejects_malformed_xml() {
    assert!(matches!(
        DashManifest::new("<MPD><Period>"),
        Err(Error::DashManifestParse(_))
    ));
}

// Tidal-style manifest: `group="main"` is not an integer, which upstream dash-mpd-core 0.1.0
// rejects. The vendored copy ignores the non-conformant value instead.
#[test]
fn parses_sample_manifest() {
    let manifest = DashManifest::new(
        r#"<?xml version='1.0' encoding='UTF-8'?>
        <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-main:2011"
            type="static" minBufferTime="PT3.993S" mediaPresentationDuration="PT5M37.56S">
            <Period id="0">
                <AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" lang="und" group="main" segmentAlignment="true">
                    <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main" />
                    <Representation id="FLAC,44100,16" codecs="flac" bandwidth="939780" audioSamplingRate="44100">
                        <SegmentTemplate timescale="44100"
                            initialization="https://media.example/track/0.mp4?token=abc"
                            media="https://media.example/track/$Number$.mp4?token=abc"
                            startNumber="1">
                            <SegmentTimeline>
                                <S d="176128" r="83" />
                                <S d="91644" />
                            </SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>"#,
    )
    .unwrap();
    assert_eq!(urls(&manifest, "FLAC").1.len(), 85);
    assert_eq!(manifest.mime_type("FLAC").unwrap(), Some("audio/mp4"));
    assert_eq!(manifest.protection_scheme("FLAC").unwrap(), None);
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

#[test]
fn resolves_base_url_chain_and_duration_template() {
    let xml = r#"
        <MPD mediaPresentationDuration="PT9S">
            <BaseURL>../media/</BaseURL>
            <Period>
                <BaseURL>audio/</BaseURL>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac" bandwidth="1411200">
                        <BaseURL>flac/</BaseURL>
                        <SegmentTemplate
                            timescale="1000"
                            duration="4000"
                            initialization="$RepresentationID$-init.mp4"
                            media="$Bandwidth$/$Number%03d$.m4s"
                        />
                    </Representation>
                    <Representation id="AACLC" codecs="mp4a.40.2">
                        <BaseURL>https://other.example/aac/</BaseURL>
                        <SegmentTemplate timescale="1000" duration="9000" initialization="i.mp4" media="$Time$.m4s"/>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url("https://cdn.example/manifests/track.mpd?sig=1")
        .unwrap();
    assert_eq!(
        manifest.base_url(),
        Some("https://cdn.example/manifests/track.mpd?sig=1")
    );
    let (init, media) = urls(&manifest, "FLAC");
    assert_eq!(init, "https://cdn.example/media/audio/flac/FLAC-init.mp4");
    assert_eq!(
        media,
        [
            "https://cdn.example/media/audio/flac/1411200/001.m4s",
            "https://cdn.example/media/audio/flac/1411200/002.m4s",
            "https://cdn.example/media/audio/flac/1411200/003.m4s",
        ]
    );
    assert_eq!(timescale(&manifest, "FLAC"), 1000);
    assert_eq!(durations(&manifest, "FLAC"), [4000, 4000, 1000]);
    let (init, media) = urls(&manifest, "AACLC");
    assert_eq!(init, "https://other.example/aac/i.mp4");
    assert_eq!(media, ["https://other.example/aac/0.m4s"]);
}

#[test]
fn relative_urls_need_a_base_url() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                            <SegmentTimeline><S d="1000"/></SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml).unwrap();
    assert!(matches!(
        manifest.segments("FLAC"),
        Err(Error::DashManifestUrl(url, url::ParseError::RelativeUrlWithoutBase)) if url == "init.mp4"
    ));
    assert!(matches!(
        manifest.with_base_url("manifest.mpd"),
        Err(Error::DashManifestUrl(
            _,
            url::ParseError::RelativeUrlWithoutBase
        ))
    ));
}

#[test]
fn period_duration_runs_to_next_period_start() {
    let xml = r#"
        <MPD mediaPresentationDuration="PT20S">
            <Period start="PT2S">
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FIRST" codecs="flac">
                        <SegmentTemplate timescale="1" initialization="https://cdn.example/i.mp4"
                            media="https://cdn.example/$Time$.mp4">
                            <SegmentTimeline><S d="2" r="-1"/></SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
            <Period start="PT7S">
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="SECOND" codecs="flac">
                        <SegmentTemplate timescale="1" duration="10" initialization="https://cdn.example/i.mp4"
                            media="https://cdn.example/$Number$.mp4"/>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml).unwrap();
    assert_eq!(durations(&manifest, "FIRST"), [2, 2, 1]);
    assert_eq!(durations(&manifest, "SECOND"), [10, 3]);

    // Each period's media presents from its start.
    let secs = Duration::from_secs;
    let first = manifest.segments("FIRST").unwrap();
    assert_eq!(first.span(0), Some(secs(2)..secs(4)));
    assert_eq!(first.end(), Some(secs(7)));
    let second = manifest.segments("SECOND").unwrap();
    assert_eq!(second.span(0), Some(secs(7)..secs(17)));
    assert_eq!(second.end(), Some(secs(20)));
}

#[test]
#[cfg(feature = "sync")]
fn new_from_url_resolves_against_redirected_url() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let xml = r#"<MPD>
        <Period>
            <AdaptationSet mimeType="audio/mp4">
                <Representation id="FLAC" codecs="flac">
                    <SegmentTemplate initialization="init.mp4" media="$Number$.mp4">
                        <SegmentTimeline><S d="1000"/></SegmentTimeline>
                    </SegmentTemplate>
                </Representation>
            </AdaptationSet>
        </Period>
    </MPD>"#;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 2 {
                header.clear();
            }
            let response = if request_line.starts_with("GET /start ") {
                "HTTP/1.1 302 Found\r\nLocation: /tracks/7/manifest.mpd\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/dash+xml\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{xml}",
                    xml.len()
                )
            };
            reader.get_mut().write_all(response.as_bytes()).unwrap();
        }
    });

    let manifest = DashManifest::new_from_url(&format!("http://{address}/start")).unwrap();
    server.join().unwrap();
    assert_eq!(
        manifest.base_url(),
        Some(format!("http://{address}/tracks/7/manifest.mpd").as_str())
    );
    let (init, media) = urls(&manifest, "FLAC");
    assert_eq!(init, format!("http://{address}/tracks/7/init.mp4"));
    assert_eq!(media, [format!("http://{address}/tracks/7/1.mp4")]);
}

#[test]
#[cfg(feature = "sync")]
fn new_from_url_accepts_data_urls() {
    let xml = r#"<MPD><Period><AdaptationSet><Representation id="AACLC"/></AdaptationSet></Period></MPD>"#;
    let encoded = STANDARD.encode(xml);
    let manifest =
        DashManifest::new_from_url(&format!("data:application/dash+xml;base64,{encoded}")).unwrap();
    assert!(manifest.representation("AACLC").is_some());
    assert_eq!(manifest.base_url(), None);
}

pub(crate) fn ranged(url: &str, start: u64, end: u64) -> SegmentLocation {
    SegmentLocation::with_range(url, start..=end)
}

#[test]
fn segment_list_media_ranges_address_one_file() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <BaseURL>track.mp4</BaseURL>
                        <SegmentList timescale="1000" duration="4000">
                            <Initialization range="0-837"/>
                            <SegmentURL mediaRange="838-50000"/>
                            <SegmentURL mediaRange="50001-90000"/>
                        </SegmentList>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url("https://cdn.example/audio/manifest.mpd")
        .unwrap();
    assert_eq!(
        parts(&manifest.segments("FLAC").unwrap()),
        (
            ranged("https://cdn.example/audio/track.mp4", 0, 837),
            vec![
                ranged("https://cdn.example/audio/track.mp4", 838, 50_000),
                ranged("https://cdn.example/audio/track.mp4", 50_001, 90_000),
            ]
        )
    );
    assert_eq!(timescale(&manifest, "FLAC"), 1000);
    assert_eq!(durations(&manifest, "FLAC"), [4000, 4000]);
}

#[test]
fn segment_list_presents_from_its_inherited_offset() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <SegmentList timescale="1000" presentationTimeOffset="500"/>
                    <Representation id="FLAC" codecs="flac">
                        <SegmentList>
                            <Initialization sourceURL="https://cdn.example/init.mp4"/>
                            <SegmentTimeline><S t="1500" d="2000" r="1"/></SegmentTimeline>
                            <SegmentURL media="https://cdn.example/1.m4s"/>
                            <SegmentURL media="https://cdn.example/2.m4s"/>
                        </SegmentList>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let segments = DashManifest::new(xml).unwrap().segments("FLAC").unwrap();
    assert_eq!(segments.timing().unwrap().presentation_time_offset, 500);
    let secs = Duration::from_secs;
    assert_eq!(segments.span(0), Some(secs(1)..secs(3)));
    assert_eq!(segments.span(1), Some(secs(3)..secs(5)));
}

#[test]
fn segment_list_inherits_initialization_and_timeline() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <SegmentList timescale="100">
                        <Initialization sourceURL="init.mp4"/>
                        <SegmentTimeline><S d="200" r="1"/><S d="50"/></SegmentTimeline>
                    </SegmentList>
                    <Representation id="AACLC" codecs="mp4a.40.2">
                        <SegmentList>
                            <SegmentURL media="a.m4s"/>
                            <SegmentURL media="b.m4s"/>
                            <SegmentURL media="c.m4s"/>
                        </SegmentList>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url("https://cdn.example/aac/")
        .unwrap();
    let (init, media) = urls(&manifest, "AACLC");
    assert_eq!(init, "https://cdn.example/aac/init.mp4");
    assert_eq!(
        media,
        [
            "https://cdn.example/aac/a.m4s",
            "https://cdn.example/aac/b.m4s",
            "https://cdn.example/aac/c.m4s",
        ]
    );
    assert_eq!(durations(&manifest, "AACLC"), [200, 200, 50]);
}

#[test]
fn segment_list_drops_urls_past_the_period() {
    let xml = r#"
        <MPD mediaPresentationDuration="PT5S">
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <SegmentList timescale="1" duration="2">
                            <Initialization sourceURL="https://cdn.example/init.mp4"/>
                            <SegmentURL media="https://cdn.example/1.m4s"/>
                            <SegmentURL media="https://cdn.example/2.m4s"/>
                            <SegmentURL media="https://cdn.example/3.m4s"/>
                            <SegmentURL media="https://cdn.example/4.m4s"/>
                        </SegmentList>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml).unwrap();
    let (init, media) = urls(&manifest, "FLAC");
    assert_eq!(init, "https://cdn.example/init.mp4");
    assert_eq!(
        media,
        [
            "https://cdn.example/1.m4s",
            "https://cdn.example/2.m4s",
            "https://cdn.example/3.m4s",
        ]
    );
    assert_eq!(durations(&manifest, "FLAC"), [2, 2, 1]);
}

#[test]
fn rejects_invalid_segment_list() {
    let manifest = |list: &str| {
        DashManifest::new(format!(
            r#"<MPD><Period><AdaptationSet><Representation id="FLAC">{list}</Representation></AdaptationSet></Period></MPD>"#
        ))
        .unwrap()
        .with_base_url("https://cdn.example/")
        .unwrap()
    };
    assert!(matches!(
        manifest(r#"<SegmentList duration="1"><Initialization/></SegmentList>"#).segments("FLAC"),
        Err(Error::DashManifestInvalidSegments(_))
    ));
    assert!(matches!(
        manifest(r#"<SegmentList duration="1"><SegmentURL media="a.m4s"/></SegmentList>"#)
            .segments("FLAC"),
        Err(Error::DashManifestMissingUrls)
    ));
    assert!(matches!(
        manifest(
            r#"<SegmentList duration="1"><Initialization/><SegmentURL mediaRange="9-1"/></SegmentList>"#
        )
        .segments("FLAC"),
        Err(Error::InvalidByteRange(range)) if range == "9-1"
    ));
}

#[test]
fn segment_template_falls_back_to_initialization_element() {
    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <BaseURL>https://cdn.example/track.mp4</BaseURL>
                        <SegmentTemplate media="$Number$.m4s">
                            <Initialization range="0-99"/>
                            <SegmentTimeline><S d="1"/></SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml).unwrap();
    assert_eq!(
        parts(&manifest.segments("FLAC").unwrap()),
        (
            ranged("https://cdn.example/track.mp4", 0, 99),
            vec![SegmentLocation::new("https://cdn.example/1.m4s")]
        )
    );
}

#[test]
#[cfg(feature = "sync")]
fn segment_stream_with_cache_shares_cached_segments() {
    use crate::segment::{SegmentCache, Transport};
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    /// Serves each URL's last path segment as its body and records requests.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl Transport for Recording {
        fn get(
            &self,
            segment: &SegmentLocation,
            _size_hint: Option<u64>,
        ) -> Result<crate::Bytes, Error> {
            self.0.lock().unwrap().push(segment.url.clone());
            let name = segment.url.rsplit('/').next().unwrap();
            Ok(crate::Bytes::copy_from_slice(name.as_bytes()))
        }
    }

    let xml = r#"
        <MPD>
            <Period>
                <AdaptationSet mimeType="audio/mp4">
                    <Representation id="FLAC" codecs="flac">
                        <SegmentTemplate initialization="init" media="$Number$">
                            <SegmentTimeline><S d="1" r="1"/></SegmentTimeline>
                        </SegmentTemplate>
                    </Representation>
                </AdaptationSet>
            </Period>
        </MPD>
    "#;
    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url("https://cdn.example/")
        .unwrap();
    let transport = Arc::new(Recording::default());
    let cache = Arc::new(SegmentCache::new(transport.clone()));
    for _ in 0..2 {
        let mut reader = manifest
            .segment_stream_with_cache("FLAC", cache.clone())
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"init12");
    }
    assert_eq!(
        *transport.0.lock().unwrap(),
        [
            "https://cdn.example/init",
            "https://cdn.example/1",
            "https://cdn.example/2",
        ]
    );
}
