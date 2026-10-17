use super::*;

/// The init URL and the media URLs of a representation without byte ranges.
fn urls(manifest: &DashManifest, id: &str) -> (String, Vec<String>) {
    let url = |fragment: Fragment| {
        assert_eq!(fragment.range, None);
        fragment.url
    };
    let fragments = manifest.fragments(id).unwrap();
    (
        url(fragments.init),
        fragments.media.into_iter().map(url).collect(),
    )
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
        manifest.fragments("FLAC"),
        Err(Error::DashManifestMissingDuration)
    ));
    assert!(matches!(
        manifest.fragments("AACLC"),
        Err(Error::DashManifestMissingTimeline)
    ));
    assert!(matches!(
        manifest.media_timeline("AACLC"),
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
    assert_eq!(
        manifest.media_timeline("AACLC").unwrap(),
        MediaTimeline {
            timescale: 44_100,
            origin: 0,
            media_durations: vec![176_128, 176_128, 176_128, 108_735],
        }
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
    assert_eq!(
        manifest.media_timeline("FLAC").unwrap(),
        MediaTimeline {
            timescale: 1000,
            origin: 0,
            media_durations: vec![2000, 2000],
        }
    );
}

#[test]
fn media_timeline_origin_is_the_first_segment_media_time() {
    let manifest = |timing: &str| {
        DashManifest::new(format!(
            r#"<MPD mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="audio/mp4">
                <Representation id="FLAC">
                    <SegmentTemplate initialization="init" media="$Number$" {timing}
                </Representation>
            </AdaptationSet></Period></MPD>"#
        ))
        .unwrap()
        .media_timeline("FLAC")
        .unwrap()
    };

    // A timeline starting after the presentation time offset.
    let timeline = manifest(
        r#"timescale="1000" presentationTimeOffset="500">
            <SegmentTimeline><S t="700" d="2000" r="1"/></SegmentTimeline>
        </SegmentTemplate>"#,
    );
    assert_eq!(timeline.origin, 700);
    // The Period ends at media time 4500, cutting the second segment short.
    assert_eq!(timeline.media_durations, [2000, 1800]);

    // Fixed-duration segments start at the offset.
    let timeline = manifest(r#"timescale="1000" presentationTimeOffset="500" duration="2000"/>"#);
    assert_eq!(timeline.origin, 500);

    // Without either, media time starts at zero.
    let timeline = manifest(
        r#"timescale="1000"><SegmentTimeline><S d="2000"/></SegmentTimeline></SegmentTemplate>"#,
    );
    assert_eq!(timeline.origin, 0);
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
    let error = manifest.stream("FLAC").unwrap_err();
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
    assert_eq!(
        manifest.media_timeline("FLAC").unwrap(),
        MediaTimeline {
            timescale: 1000,
            origin: 0,
            media_durations: vec![4000, 4000, 1000],
        }
    );
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
        manifest.fragments("FLAC"),
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
    assert_eq!(
        manifest.media_timeline("FIRST").unwrap().media_durations,
        [2, 2, 1]
    );
    assert_eq!(
        manifest.media_timeline("SECOND").unwrap().media_durations,
        [10, 3]
    );
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

pub(crate) fn ranged(url: &str, start: u64, end: u64) -> Fragment {
    Fragment::with_range(url, start..=end)
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
        manifest.fragments("FLAC").unwrap(),
        Fragments::new(
            ranged("https://cdn.example/audio/track.mp4", 0, 837),
            [
                ranged("https://cdn.example/audio/track.mp4", 838, 50_000),
                ranged("https://cdn.example/audio/track.mp4", 50_001, 90_000),
            ]
        )
    );
    assert_eq!(
        manifest.media_timeline("FLAC").unwrap(),
        MediaTimeline {
            timescale: 1000,
            origin: 0,
            media_durations: vec![4000, 4000],
        }
    );
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
    assert_eq!(
        manifest.media_timeline("AACLC").unwrap().media_durations,
        [200, 200, 50]
    );
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
    assert_eq!(
        manifest.media_timeline("FLAC").unwrap().media_durations,
        [2, 2, 1]
    );
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
        manifest(r#"<SegmentList duration="1"><Initialization/></SegmentList>"#).fragments("FLAC"),
        Err(Error::DashManifestInvalidSegments(_))
    ));
    assert!(matches!(
        manifest(r#"<SegmentList duration="1"><SegmentURL media="a.m4s"/></SegmentList>"#)
            .fragments("FLAC"),
        Err(Error::DashManifestMissingUrls)
    ));
    assert!(matches!(
        manifest(
            r#"<SegmentList duration="1"><Initialization/><SegmentURL mediaRange="9-1"/></SegmentList>"#
        )
        .fragments("FLAC"),
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
        manifest.fragments("FLAC").unwrap(),
        Fragments::new(
            ranged("https://cdn.example/track.mp4", 0, 99),
            ["https://cdn.example/1.m4s"]
        )
    );
}

#[test]
#[cfg(feature = "sync")]
fn stream_with_cache_shares_cached_fragments() {
    use crate::stream::{FragmentCache, Transport};
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    /// Serves each URL's last path segment as its body and records requests.
    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl Transport for Recording {
        fn get(&self, fragment: &Fragment, _size_hint: Option<u64>) -> Result<crate::Bytes, Error> {
            self.0.lock().unwrap().push(fragment.url.clone());
            let name = fragment.url.rsplit('/').next().unwrap();
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
    let cache = Arc::new(FragmentCache::new(transport.clone()));
    for _ in 0..2 {
        let mut reader = manifest.stream_with_cache("FLAC", cache.clone()).unwrap();
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
