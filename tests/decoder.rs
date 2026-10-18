//! `MediaStream` against real DASH output from ffmpeg: decoder-ready packets that
//! carry the same access units and timing as `PacketStream`'s MP4 packets for every codec,
//! and, with the `ffmpeg-tests` feature, decode to the same output as the source.
#![cfg(feature = "sync")]

mod common;

use common::{Fixtures, fragment_stream, fragment_stream_through, muxed};
use playdash::fragment::{Fragment, FragmentStream};
use playdash::packet::{Packet, PacketSeeked, PacketStream};
use playdash::{
    Bytes, Codec, ContentKind, DecoderFormat, DecoderParameters, DecoderStyle, Error,
    FragmentCache, MediaPacket, MediaStream, MediaStreamBuilder, Seeked, Transport,
};
use std::sync::Arc;
use std::time::Duration;

const AUDIO: [&str; 8] = ["aac", "he_aac", "opus", "flac", "ac3", "eac3", "mp3", "pcm"];
const VIDEO: [&str; 5] = ["h264", "hevc", "hevc_open_gop", "av1", "vp9"];

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

fn packets(name: &str) -> PacketStream {
    PacketStream::new(fragment_stream(name, Arc::default())).unwrap()
}

fn media_stream(name: &str) -> MediaStream {
    packets(name).into_media_stream().unwrap()
}

/// Seeks `stream` to `time`, or to the last keyframe at or before it if `keyframe`.
fn seek(stream: &mut MediaStream, time: Duration, keyframe: bool) -> Seeked {
    match keyframe {
        false => stream.seek_time(time),
        true => stream.seek_keyframe(time),
    }
    .unwrap()
}

/// Seeks `stream` to `time`, or to the last keyframe at or before it if `keyframe`.
fn seek_packets(stream: &mut PacketStream, time: Duration, keyframe: bool) -> PacketSeeked {
    match keyframe {
        false => stream.seek_time(time),
        true => stream.seek_keyframe(time),
    }
    .unwrap()
}

/// Opens `builder` seeked to `time`, or to the last keyframe at or before it if
/// `keyframe`.
fn open_at(builder: MediaStreamBuilder, time: Duration, keyframe: bool) -> (MediaStream, Seeked) {
    match keyframe {
        false => builder.open_at(time),
        true => builder.open_at_keyframe(time),
    }
    .unwrap()
}

/// The format fixture `name` converts to.
fn format_of(name: &str) -> DecoderFormat {
    match name {
        "aac" | "he_aac" => DecoderFormat::AacRaw,
        "opus" => DecoderFormat::OpusPackets,
        "flac" => DecoderFormat::Flac,
        "ac3" => DecoderFormat::Ac3,
        "eac3" => DecoderFormat::Eac3,
        "mp3" => DecoderFormat::Mp3,
        "pcm" => DecoderFormat::Pcm,
        "h264" => DecoderFormat::H264AnnexB,
        "hevc" | "hevc_open_gop" => DecoderFormat::HevcAnnexB,
        "av1" => DecoderFormat::Av1Obu,
        "vp9" => DecoderFormat::Vp9Frames,
        _ => unreachable!("{name}"),
    }
}

/// The NAL units of `annexb`, an Annex-B byte stream with four-byte start codes.
fn nal_units(annexb: &[u8]) -> Vec<&[u8]> {
    assert!(annexb.starts_with(&START_CODE), "starts with a start code");
    let mut starts: Vec<usize> = annexb
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == START_CODE)
        .map(|(pos, _)| pos)
        .collect();
    starts.push(annexb.len());
    starts
        .windows(2)
        .map(|pair| &annexb[pair[0] + 4..pair[1]])
        .collect()
}

/// `data`, a packet in `format`, back in the form the fixtures' MP4 samples have: four-byte
/// NAL unit lengths for H.264 and HEVC, AV1 without its temporal delimiter, and the rest
/// unchanged.
fn mp4_form(format: DecoderFormat, data: &[u8]) -> Vec<u8> {
    match format {
        DecoderFormat::H264AnnexB | DecoderFormat::HevcAnnexB => {
            let mut out = Vec::new();
            for nal in nal_units(data) {
                out.extend((nal.len() as u32).to_be_bytes());
                out.extend(nal);
            }
            out
        }
        DecoderFormat::Av1Obu => data
            .strip_prefix(&TEMPORAL_DELIMITER)
            .expect("a temporal delimiter")
            .to_vec(),
        _ => data.to_vec(),
    }
}

/// Checks `converted` is `packet` in `format`, with every timing field kept.
fn assert_converted(
    context: &str,
    format: DecoderFormat,
    converted: &MediaPacket,
    packet: &Packet,
) {
    assert_eq!(
        mp4_form(format, &converted.data),
        packet.data,
        "{context} data"
    );
    assert_eq!(
        (
            converted.pts,
            converted.dts,
            converted.duration,
            converted.timescale,
            converted.keyframe,
            converted.trim_start,
            converted.trim_end
        ),
        (
            packet.pts,
            packet.dts,
            packet.duration,
            packet.timescale,
            packet.keyframe,
            packet.trim_start,
            packet.trim_end
        ),
        "{context} timing"
    );
}

/// Checks `converted` are `packets` in `format`, one for one.
fn assert_all_converted(
    name: &str,
    format: DecoderFormat,
    converted: &[MediaPacket],
    packets: &[Packet],
) {
    assert_eq!(converted.len(), packets.len(), "{name} packet count");
    for (index, (converted, packet)) in converted.iter().zip(packets).enumerate() {
        assert_converted(&format!("{name} packet {index}"), format, converted, packet);
    }
}

#[test]
fn every_codec_has_its_initialization() {
    for name in AUDIO.iter().chain(&VIDEO) {
        let stream = media_stream(name);
        let initialization = stream.initialization();
        let data = &initialization.data[..];
        assert_eq!(initialization.format, format_of(name), "{name}");
        match stream.codec() {
            Codec::H264(_) => {
                let nals = nal_units(data);
                let types: Vec<u8> = nals.iter().map(|nal| nal[0] & 0x1f).collect();
                assert_eq!(types, [7, 8], "SPS, then PPS");
                // The same units, unframed, in the parameters.
                assert!(matches!(
                    &initialization.parameters,
                    DecoderParameters::Video { vps, sps, pps, .. }
                        if vps.is_empty() && sps[..] == [nals[0]] && pps[..] == [nals[1]]
                ));
            }
            Codec::Hevc(_) => {
                let nals = nal_units(data);
                let types: Vec<u8> = nals.iter().map(|nal| nal[0] >> 1).collect();
                assert_eq!(types, [32, 33, 34, 39], "VPS, SPS, PPS, then SEI");
                assert!(matches!(
                    &initialization.parameters,
                    DecoderParameters::Video { vps, sps, pps, .. }
                        if vps[..] == [nals[0]] && sps[..] == [nals[1]] && pps[..] == [nals[2]]
                ));
            }
            Codec::Av1(video) => assert_eq!(data, &video.config[4..], "{name}"),
            Codec::Aac(aac) => assert_eq!(data, &aac.audio.config[..], "{name}"),
            Codec::Ac3(audio) | Codec::Eac3(audio) => assert_eq!(data, &audio.config[..]),
            Codec::Flac(audio) => {
                assert_eq!(&data[..4], b"fLaC");
                assert_eq!(&data[4..], &audio.config[4..]);
            }
            Codec::Opus(_) => {
                let mut head = b"OpusHead".to_vec();
                head.extend([1, 1, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 0]);
                assert_eq!(data, head);
            }
            Codec::Vp9(video) => assert_eq!(data, &video.config[..], "{name}"),
            Codec::Mp3(_) | Codec::Pcm(_) => assert!(data.is_empty(), "{name}"),
            other => panic!("{name}: {other:?}"),
        }

        let parameters = &initialization.parameters;
        let expected = match *name {
            "flac" => {
                let Codec::Flac(audio) = stream.codec() else {
                    unreachable!()
                };
                matches!(
                    parameters,
                    DecoderParameters::Flac {
                        sample_rate: 22_050,
                        channels: 1,
                        bits_per_sample: 16,
                        streaminfo,
                        ..
                    } if streaminfo[..] == audio.config[8..42]
                )
            }
            "opus" => matches!(
                parameters,
                DecoderParameters::Opus {
                    channels: 1,
                    pre_skip: 312,
                    output_gain: 0,
                    mapping_family: 0,
                    stream_count: 1,
                    coupled_count: 0,
                    channel_mapping,
                    ..
                } if channel_mapping == &[0]
            ),
            "pcm" => matches!(
                parameters,
                DecoderParameters::Pcm {
                    sample_rate: 8_000,
                    channels: 1,
                    bits_per_sample: 16,
                    float: false,
                    little_endian: true,
                    ..
                }
            ),
            _ if VIDEO.contains(name) => matches!(
                parameters,
                DecoderParameters::Video {
                    width: 96,
                    height: 64,
                    ..
                }
            ),
            _ => matches!(
                parameters,
                DecoderParameters::Audio {
                    sample_rate: 48_000,
                    channels: 1,
                    ..
                }
            ),
        };
        assert!(expected, "{name}: {parameters:?}");
    }
}

#[test]
fn every_codec_converts_the_mp4_packets() {
    for name in AUDIO.iter().chain(&VIDEO) {
        let converted: Vec<MediaPacket> = media_stream(name).map(Result::unwrap).collect();
        let packets: Vec<Packet> = packets(name).map(Result::unwrap).collect();
        assert!(packets.len() > 10, "{name}");
        assert_all_converted(name, format_of(name), &converted, &packets);
    }
}

#[test]
fn seek_preroll_is_converted_like_every_other_packet() {
    for name in AUDIO.iter().chain(&VIDEO) {
        for (time, keyframe) in [
            (Duration::from_millis(1_300), false),
            (Duration::from_millis(700), false),
            (Duration::from_millis(600), true),
        ] {
            let context = format!("{name} {time:?} keyframe {keyframe}");
            let format = format_of(name);
            let mut stream = media_stream(name);
            let mut plain = packets(name);

            let seeked = seek(&mut stream, time, keyframe);
            let expected = seek_packets(&mut plain, time, keyframe);
            assert_eq!(
                (seeked.requested, seeked.landed, seeked.discard_before),
                (expected.requested, expected.landed, expected.discard_before),
                "{context}"
            );
            assert_all_converted(&context, format, &seeked.preroll, &expected.preroll);

            let rest: Vec<MediaPacket> = stream.map(Result::unwrap).collect();
            let expected: Vec<Packet> = plain.map(Result::unwrap).collect();
            assert_all_converted(&context, format, &rest, &expected);
        }
    }
}

/// The format fixture `name` converts to in [`DecoderStyle::ConfigRecord`].
fn config_record_format_of(name: &str) -> DecoderFormat {
    match name {
        "h264" => DecoderFormat::H264LengthPrefixed,
        "hevc" | "hevc_open_gop" => DecoderFormat::HevcLengthPrefixed,
        "av1" => DecoderFormat::Av1Samples,
        _ => format_of(name),
    }
}

#[test]
fn config_record_style_gives_the_mp4_packets_with_their_record() {
    for name in AUDIO.iter().chain(&VIDEO) {
        let format = config_record_format_of(name);
        let mut stream = packets(name)
            .into_media_stream_for(DecoderStyle::ConfigRecord)
            .unwrap();
        let initialization = stream.initialization().clone();
        assert_eq!(initialization.format, format, "{name}");
        match stream.codec() {
            // The configuration record itself.
            Codec::H264(video) | Codec::Hevc(video) | Codec::Av1(video) => {
                assert_eq!(initialization.data, video.config, "{name}");
                assert!(
                    matches!(
                        initialization.parameters,
                        DecoderParameters::Video {
                            width: 96,
                            height: 64,
                            ..
                        }
                    ),
                    "{name}"
                );
            }
            // One format serves both styles.
            _ => assert_eq!(
                &initialization,
                media_stream(name).initialization(),
                "{name}"
            ),
        }

        // Asking for the format itself gives the same stream.
        let exact = packets(name).into_media_stream_as(format).unwrap();
        assert_eq!(exact.initialization(), &initialization, "{name}");

        let converted: Vec<MediaPacket> = stream.by_ref().map(Result::unwrap).collect();
        let expected: Vec<Packet> = packets(name).map(Result::unwrap).collect();
        assert_all_converted(name, format, &converted, &expected);

        let time = Duration::from_millis(1_300);
        let seeked = stream.seek_time(time).unwrap();
        let expected = packets(name).seek_time(time).unwrap();
        assert_all_converted(name, format, &seeked.preroll, &expected.preroll);
    }
}

#[test]
fn adts_puts_a_header_on_every_aac_access_unit() {
    for name in ["aac", "he_aac"] {
        let mut stream = packets(name)
            .into_media_stream_as(DecoderFormat::AacAdts)
            .unwrap();
        assert_eq!(stream.initialization().format, DecoderFormat::AacAdts);
        assert!(stream.initialization().data.is_empty(), "{name}");

        // Each packet: a 7-byte header whose frame length counts it, then the access unit.
        let converted: Vec<MediaPacket> = stream.by_ref().map(Result::unwrap).collect();
        let expected: Vec<Packet> = packets(name).map(Result::unwrap).collect();
        assert_eq!(converted.len(), expected.len(), "{name}");
        for (index, (converted, packet)) in converted.iter().zip(&expected).enumerate() {
            let (header, body) = converted.data.split_at(7);
            let len = usize::from(header[3] & 0x03) << 11
                | usize::from(header[4]) << 3
                | usize::from(header[5]) >> 5;
            assert_eq!(&header[..2], [0xff, 0xf1], "{name} packet {index}");
            assert_eq!(len, converted.data.len(), "{name} packet {index}");
            assert_eq!(body, &packet.data[..], "{name} packet {index}");
            assert_eq!(converted.pts, packet.pts, "{name} packet {index}");
        }
    }
}

#[test]
fn both_tracks_of_a_muxed_representation_convert() {
    let cache = Arc::new(FragmentCache::new(Arc::new(Fixtures::default())));
    let stream = || FragmentStream::new_with_cache(muxed(), cache.clone()).unwrap();
    for (track, format) in [(1, DecoderFormat::H264AnnexB), (2, DecoderFormat::AacRaw)] {
        let converted = PacketStream::with_track(stream(), track)
            .unwrap()
            .into_media_stream()
            .unwrap();
        assert_eq!(converted.initialization().format, format);
        let converted: Vec<MediaPacket> = converted.map(Result::unwrap).collect();
        let packets: Vec<Packet> = PacketStream::with_track(stream(), track)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_all_converted(
            &format!("muxed track {track}"),
            format,
            &converted,
            &packets,
        );
    }
}

#[test]
fn mismatched_formats_are_refused() {
    let error = packets("h264")
        .into_media_stream_as(DecoderFormat::AacRaw)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::UnsupportedDecoderInput { codec, format: Some(DecoderFormat::AacRaw), .. }
                if codec == "H.264"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "H.264 cannot be decoder input in raw AAC: H.264 converts only to H.264 Annex B or \
         length-prefixed H.264"
    );
    assert!(matches!(
        packets("aac").into_media_stream_as(DecoderFormat::Mp3),
        Err(Error::UnsupportedDecoderInput { codec, .. }) if codec == "AAC"
    ));

    for name in AUDIO.iter().chain(&VIDEO) {
        let explicit = packets(name).into_media_stream_as(format_of(name)).unwrap();
        assert_eq!(
            explicit.initialization(),
            media_stream(name).initialization(),
            "{name}"
        );
    }
}

/// Serves fixtures like [`Fixtures`], except that the first sample of `h264_2.m4s` claims
/// a NAL unit longer than the sample: valid MP4 holding H.264 that does not follow its
/// avcC.
#[derive(Default)]
struct CorruptH264(Fixtures);

impl Transport for CorruptH264 {
    fn get(&self, fragment: &Fragment, size_hint: Option<u64>) -> Result<Bytes, Error> {
        let body = self.0.get(fragment, size_hint)?;
        if !fragment.url.ends_with("h264_2.m4s") {
            return Ok(body);
        }
        // The first sample starts the mdat's payload, with its first NAL unit's length.
        let mut body = body.to_vec();
        let mut pos = 0;
        while &body[pos + 4..pos + 8] != b"mdat" {
            pos += u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
        }
        body[pos + 8..pos + 12].copy_from_slice(&[0x7f, 0xff, 0xff, 0xff]);
        Ok(Bytes::from(body))
    }
}

#[test]
fn a_malformed_packet_ends_iteration_until_a_seek() {
    let fragments = fragment_stream_through("h264", Arc::new(CorruptH264::default()));
    let mut stream = PacketStream::new(fragments)
        .unwrap()
        .into_media_stream()
        .unwrap();

    // The first segment's packets, then the error, once.
    let first_segment = stream.by_ref().take_while(Result::is_ok).count();
    assert_eq!(first_segment, 25);
    assert!(stream.next().is_none(), "iteration ended after the error");

    // The error itself, from a fresh stream.
    let fragments = fragment_stream_through("h264", Arc::new(CorruptH264::default()));
    let mut fresh = PacketStream::new(fragments)
        .unwrap()
        .into_media_stream()
        .unwrap();
    let error = fresh.find_map(Result::err).unwrap();
    assert!(
        matches!(
            &error,
            Error::MalformedDecoderInput { codec, format: DecoderFormat::H264AnnexB, reason }
                if codec == "H.264" && reason.starts_with("NAL unit at byte 0 declares 2147483647 bytes")
        ),
        "{error:?}"
    );

    // A seek starts iteration again, up to the same packet.
    let seeked = stream.seek_time(Duration::ZERO).unwrap();
    assert!(seeked.preroll.is_empty());
    assert_eq!(stream.by_ref().take_while(Result::is_ok).count(), 25);
    assert!(stream.next().is_none());
}

/// A cache that fetches through fresh [`Fixtures`].
fn fixtures_cache() -> Arc<FragmentCache> {
    Arc::new(FragmentCache::new(Arc::new(Fixtures::default())))
}

/// `open` applied to a builder of fixture `name`'s representation that fetches through
/// fresh [`Fixtures`].
fn built<T>(name: &str, open: impl FnOnce(MediaStreamBuilder<'_>) -> T) -> T {
    let manifest = common::manifest(name);
    open(manifest.media_stream("0").cache(fixtures_cache()))
}

/// Asserts that `stream` gives the same initialization and packets as `expected`.
fn assert_same_stream(context: &str, stream: MediaStream, expected: MediaStream) {
    assert_eq!(
        stream.initialization(),
        expected.initialization(),
        "{context}"
    );
    let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect();
    let expected: Vec<MediaPacket> = expected.map(Result::unwrap).collect();
    assert!(!packets.is_empty(), "{context}");
    assert_eq!(packets, expected, "{context}");
}

#[test]
fn the_manifest_builder_opens_what_into_media_stream_gives() {
    for name in AUDIO.iter().chain(&VIDEO) {
        let stream = built(name, |builder| builder.open().unwrap());
        assert_same_stream(name, stream, media_stream(name));
    }
}

#[test]
fn the_manifest_builder_converts_to_the_style_or_format_asked_for() {
    for name in AUDIO.iter().chain(&VIDEO) {
        let stream = built(name, |builder| {
            builder.style(DecoderStyle::ConfigRecord).open().unwrap()
        });
        let expected = packets(name)
            .into_media_stream_for(DecoderStyle::ConfigRecord)
            .unwrap();
        assert_same_stream(name, stream, expected);
    }

    let adts = built("aac", |builder| {
        builder.format(DecoderFormat::AacAdts).open().unwrap()
    });
    let expected = packets("aac")
        .into_media_stream_as(DecoderFormat::AacAdts)
        .unwrap();
    assert_same_stream("aac adts", adts, expected);

    // The last of a style and a format wins.
    let raw = built("aac", |builder| {
        builder
            .format(DecoderFormat::AacAdts)
            .style(DecoderStyle::Elementary)
            .open()
            .unwrap()
    });
    assert_eq!(raw.initialization().format, DecoderFormat::AacRaw);

    let error = built("h264", |builder| {
        builder.format(DecoderFormat::AacRaw).open()
    })
    .unwrap_err();
    assert!(
        matches!(
            error,
            Error::UnsupportedDecoderInput {
                format: Some(DecoderFormat::AacRaw),
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn open_at_seeks_as_open_then_a_seek_does() {
    for name in AUDIO.iter().chain(&VIDEO) {
        for (time, keyframe) in [
            (Duration::from_millis(1_010), false),
            (Duration::from_millis(1_300), false),
            (Duration::from_millis(600), true),
        ] {
            let context = format!("{name} {time:?} keyframe {keyframe}");
            let (stream, seeked) = built(name, |builder| open_at(builder, time, keyframe));
            let mut expected = media_stream(name);
            assert_eq!(seeked, seek(&mut expected, time, keyframe), "{context}");
            assert_same_stream(&context, stream, expected);
        }
    }
}

#[test]
fn open_at_takes_preroll_from_an_earlier_fragment() {
    // Opus's second fragment spans ticks 23688 to 47688 at 48 kHz, and its third holds
    // 1.01s; the 80ms of pre-roll before 1.01s starts in the second.
    let (stream, seeked) = built("opus", |builder| {
        builder.open_at(Duration::from_millis(1_010)).unwrap()
    });
    assert_eq!(stream.timescale(), 48_000);
    let first = seeked.preroll.first().expect("pre-roll");
    assert!(
        (23_688..47_688).contains(&first.pts),
        "pre-roll starts at {}",
        first.pts
    );
}

#[test]
fn the_manifest_builder_sets_the_preroll() {
    let time = Duration::from_millis(1_300);
    let (_, seeked) = built("opus", |builder| {
        builder.preroll(Duration::ZERO).open_at(time).unwrap()
    });
    let mut expected = packets("opus")
        .with_preroll(Duration::ZERO)
        .into_media_stream()
        .unwrap();
    assert_eq!(seeked, expected.seek_time(time).unwrap());

    let (_, default) = built("opus", |builder| builder.open_at(time).unwrap());
    assert!(seeked.preroll.len() < default.preroll.len());
}

#[test]
fn the_manifest_builder_reads_a_muxed_track_by_kind_or_id() {
    let cache = fixtures_cache();
    let track_stream = |track| {
        let fragments = FragmentStream::new_with_cache(muxed(), cache.clone()).unwrap();
        PacketStream::with_track(fragments, track)
            .unwrap()
            .into_media_stream()
            .unwrap()
    };
    for (kind, track) in [(ContentKind::Video, 1), (ContentKind::Audio, 2)] {
        let by_kind = built("muxed", |builder| builder.track(kind).open().unwrap());
        assert_same_stream(&format!("{kind:?}"), by_kind, track_stream(track));
        let by_id = built("muxed", |builder| builder.track_id(track).open().unwrap());
        assert_same_stream(&format!("track {track}"), by_id, track_stream(track));
    }

    // The last of a kind and an ID wins.
    let audio = built("muxed", |builder| {
        builder
            .track(ContentKind::Video)
            .track_id(2)
            .open()
            .unwrap()
    });
    assert_eq!(audio.initialization().format, DecoderFormat::AacRaw);

    for error in [
        built("muxed", |builder| builder.track(ContentKind::Text).open()).unwrap_err(),
        built("muxed", |builder| builder.track_id(9).open()).unwrap_err(),
    ] {
        assert!(matches!(error, Error::Initialization(_)), "{error:?}");
    }

    // Without a track, as `PacketStream::new`.
    let error = built("muxed", |builder| builder.open()).unwrap_err();
    assert!(matches!(error, Error::UnsupportedMedia { .. }), "{error:?}");
}

/// The same decoder-ready packets and seeks through `nonblocking::MediaStream`.
#[cfg(feature = "async")]
mod nonblocking {
    use super::*;
    use common::async_fragment_stream;
    use futures_util::StreamExt;
    use playdash::nonblocking;

    async fn media_stream(name: &str) -> nonblocking::MediaStream {
        nonblocking::packet::PacketStream::new(async_fragment_stream(name).await)
            .await
            .unwrap()
            .into_media_stream()
            .unwrap()
    }

    #[tokio::test]
    async fn every_codec_matches_the_blocking_stream() {
        for name in AUDIO.iter().chain(&VIDEO) {
            let stream = media_stream(name).await;
            let blocking = super::media_stream(name);
            assert_eq!(stream.initialization(), blocking.initialization(), "{name}");
            assert_eq!(stream.codec(), blocking.codec(), "{name}");
            let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect().await;
            let expected: Vec<MediaPacket> = blocking.map(Result::unwrap).collect();
            assert_eq!(packets, expected, "{name}");
        }
    }

    #[tokio::test]
    async fn seeks_match_the_blocking_stream() {
        for name in AUDIO.iter().chain(&VIDEO) {
            for (time, keyframe) in [
                (Duration::from_millis(1_300), false),
                (Duration::from_millis(600), true),
            ] {
                let context = format!("{name} {time:?} keyframe {keyframe}");
                let mut stream = media_stream(name).await;
                let mut blocking = super::media_stream(name);
                let seeked = match keyframe {
                    false => stream.seek_time(time).await,
                    true => stream.seek_keyframe(time).await,
                };
                assert_eq!(
                    seeked.unwrap(),
                    seek(&mut blocking, time, keyframe),
                    "{context}"
                );
                let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect().await;
                let expected: Vec<MediaPacket> = blocking.map(Result::unwrap).collect();
                assert_eq!(packets, expected, "{context}");
            }
        }
    }

    #[tokio::test]
    async fn config_record_style_matches_the_blocking_stream() {
        for name in AUDIO.iter().chain(&VIDEO) {
            let stream = nonblocking::packet::PacketStream::new(async_fragment_stream(name).await)
                .await
                .unwrap()
                .into_media_stream_for(DecoderStyle::ConfigRecord)
                .unwrap();
            let blocking = super::packets(name)
                .into_media_stream_for(DecoderStyle::ConfigRecord)
                .unwrap();
            assert_eq!(stream.initialization(), blocking.initialization(), "{name}");
            let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect().await;
            let expected: Vec<MediaPacket> = blocking.map(Result::unwrap).collect();
            assert_eq!(packets, expected, "{name}");
        }
    }

    /// Fixture `name`'s manifest, for the async API.
    fn manifest(name: &str) -> nonblocking::DashManifest {
        nonblocking::DashManifest::from(common::manifest(name))
    }

    /// A cache that fetches through fresh [`Fixtures`], for the async API.
    fn fixtures_cache() -> Arc<nonblocking::fragment::FragmentCache> {
        Arc::new(nonblocking::fragment::FragmentCache::new(Arc::new(
            Fixtures::default(),
        )))
    }

    /// Asserts that `stream` gives the same initialization and packets as the blocking
    /// `expected`.
    async fn assert_same_stream(
        context: &str,
        stream: nonblocking::MediaStream,
        expected: MediaStream,
    ) {
        assert_eq!(
            stream.initialization(),
            expected.initialization(),
            "{context}"
        );
        let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect().await;
        let expected: Vec<MediaPacket> = expected.map(Result::unwrap).collect();
        assert!(!packets.is_empty(), "{context}");
        assert_eq!(packets, expected, "{context}");
    }

    #[tokio::test]
    async fn the_manifest_builder_matches_the_blocking_builder() {
        for name in AUDIO.iter().chain(&VIDEO) {
            let stream = manifest(name)
                .media_stream("0")
                .cache(fixtures_cache())
                .style(DecoderStyle::ConfigRecord)
                .open()
                .await
                .unwrap();
            let expected = built(name, |builder| {
                builder.style(DecoderStyle::ConfigRecord).open().unwrap()
            });
            assert_same_stream(name, stream, expected).await;
        }
    }

    #[tokio::test]
    async fn open_at_matches_the_blocking_open_at() {
        for name in AUDIO.iter().chain(&VIDEO) {
            for (time, keyframe) in [
                (Duration::from_millis(1_010), false),
                (Duration::from_millis(600), true),
            ] {
                let context = format!("{name} {time:?} keyframe {keyframe}");
                let manifest = manifest(name);
                let builder = manifest.media_stream("0").cache(fixtures_cache());
                let (stream, seeked) = match keyframe {
                    false => builder.open_at(time).await,
                    true => builder.open_at_keyframe(time).await,
                }
                .unwrap();
                let (expected, expected_seeked) =
                    built(name, |builder| open_at(builder, time, keyframe));
                assert_eq!(seeked, expected_seeked, "{context}");
                assert_same_stream(&context, stream, expected).await;
            }
        }
    }

    #[tokio::test]
    async fn the_manifest_builder_reads_a_muxed_track_by_kind() {
        let manifest = manifest("muxed");
        for kind in [ContentKind::Video, ContentKind::Audio] {
            let stream = manifest
                .media_stream("0")
                .cache(fixtures_cache())
                .track(kind)
                .open()
                .await
                .unwrap();
            let expected = built("muxed", |builder| builder.track(kind).open().unwrap());
            assert_same_stream(&format!("{kind:?}"), stream, expected).await;
        }
        let opened = manifest
            .media_stream("0")
            .cache(fixtures_cache())
            .track(ContentKind::Text)
            .open()
            .await;
        assert!(matches!(opened, Err(Error::Initialization(_))));
    }

    #[tokio::test]
    async fn mismatched_formats_are_refused() {
        let packets = nonblocking::packet::PacketStream::new(async_fragment_stream("h264").await)
            .await
            .unwrap();
        assert!(matches!(
            packets.into_media_stream_as(DecoderFormat::Flac),
            Err(Error::UnsupportedDecoderInput {
                format: Some(DecoderFormat::Flac),
                ..
            })
        ));
    }
}

/// The decoder-ready packets decoded by ffmpeg, compared with ffmpeg decoding the source.
#[cfg(feature = "ffmpeg-tests")]
mod ffmpeg {
    use super::*;
    use common::fixture;
    use ffmpeg_sidecar::command::FfmpegCommand;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::thread;

    /// The hash of each frame ffmpeg decodes from `input`, in presentation order, reading
    /// it with `input_args` (such as a format) and hashing with `muxer`: `framehash` for
    /// each frame, or `hash` for the whole output.
    fn frame_hashes(input_args: &[&str], input: Vec<u8>, muxer: &str) -> Vec<String> {
        let mut command = FfmpegCommand::new();
        let mut child = command
            .args(input_args)
            .input("-")
            .args(["-f", muxer, "-hash", "CRC32"])
            .output("-")
            .spawn()
            .expect("ffmpeg on PATH");

        // Feed stdin and drain stderr while reading stdout, so no pipe fills up.
        let mut stdin = child.take_stdin().unwrap();
        let writer = thread::spawn(move || stdin.write_all(&input));
        let mut stderr = child.take_stderr().unwrap();
        let log = thread::spawn(move || {
            let mut log = String::new();
            stderr.read_to_string(&mut log).map(|_| log)
        });
        let mut out = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        writer.join().unwrap().unwrap();
        let log = log.join().unwrap().unwrap();
        assert!(child.wait().unwrap().success(), "ffmpeg failed:\n{log}");

        // Each frame's line ends with its hash.
        out.lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.rsplit(',').next().unwrap().trim().to_owned())
            .collect()
    }

    /// Fixture `name` as one MP4 file: its single file, or its initialization segment
    /// then its media segments.
    fn source(name: &str) -> Vec<u8> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/files/packets");
        if dir.join(format!("{name}.mp4")).exists() {
            return fixture(&format!("packets/{name}.mp4"));
        }
        let mut mp4 = fixture(&format!("packets/{name}_init.mp4"));
        for number in 1.. {
            let segment = format!("{name}_{number}.m4s");
            if !dir.join(&segment).exists() {
                break;
            }
            mp4.extend(fixture(&format!("packets/{segment}")));
        }
        mp4
    }

    /// ADTS headers on each of `packets`, raw AAC for the AudioSpecificConfig
    /// `config`, whose object type, sampling frequency index and channel configuration
    /// must fit ADTS's fields.
    fn adts(config: &[u8], packets: &[&MediaPacket]) -> Vec<u8> {
        let object_type = config[0] >> 3;
        let frequency = (config[0] & 0x07) << 1 | config[1] >> 7;
        let channels = (config[1] >> 3) & 0x0f;
        assert!((1..=4).contains(&object_type) && frequency < 13 && channels < 8);

        let mut out = Vec::new();
        for packet in packets {
            let len = 7 + packet.data.len();
            out.extend([
                0xff,
                0xf1,
                (object_type - 1) << 6 | frequency << 2 | channels >> 2,
                (channels & 0x03) << 6 | (len >> 11) as u8,
                (len >> 3) as u8,
                ((len & 0x07) as u8) << 5 | 0x1f,
                0xfc,
            ]);
            out.extend(&packet.data);
        }
        out
    }

    /// The Ogg CRC-32: polynomial 0x04c11db7, unreflected, from zero.
    fn ogg_crc(data: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &byte in data {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    crc << 1 ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// Appends an Ogg page holding `packet` whole to `out`.
    fn ogg_page(out: &mut Vec<u8>, flags: u8, granule: i64, sequence: u32, packet: &[u8]) {
        let mut lacing = vec![255u8; packet.len() / 255];
        lacing.push((packet.len() % 255) as u8);
        let start = out.len();
        out.extend(b"OggS");
        out.extend([0, flags]);
        out.extend(granule.to_le_bytes());
        out.extend(1u32.to_le_bytes()); // serial number
        out.extend(sequence.to_le_bytes());
        out.extend([0; 4]); // CRC, filled in below
        out.push(lacing.len() as u8);
        out.extend(&lacing);
        out.extend(packet);
        let crc = ogg_crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
    }

    /// The 48 kHz samples an Opus packet decodes to, from its TOC byte (RFC 6716 §3.1).
    fn opus_samples(packet: &[u8]) -> i64 {
        let config = packet[0] >> 3;
        let frame = match config {
            0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)],
            12..=15 => [480, 960][usize::from(config % 2)],
            _ => [120, 240, 480, 960][usize::from(config % 4)],
        };
        let frames = match packet[0] & 0x03 {
            0 => 1,
            1 | 2 => 2,
            _ => i64::from(packet[1] & 0x3f),
        };
        frame * frames
    }

    /// An untrimmed Ogg Opus stream of `head`, an `OpusHead`, and `packets`, one to a
    /// page.
    fn ogg_opus(head: &[u8], packets: &[&MediaPacket]) -> Vec<u8> {
        let mut out = Vec::new();
        ogg_page(&mut out, 0x02, 0, 0, head);
        let mut tags = b"OpusTags".to_vec();
        tags.extend(0u32.to_le_bytes()); // vendor string length
        tags.extend(0u32.to_le_bytes()); // comment count
        ogg_page(&mut out, 0, 0, 1, &tags);

        // Granule positions count 48 kHz samples decoded, pre-skip included. Counting
        // every packet's whole decoded length leaves the last packet's padding in.
        let mut granule = 0i64;
        for (index, packet) in packets.iter().enumerate() {
            granule += opus_samples(&packet.data);
            let last = index + 1 == packets.len();
            let flags = if last { 0x04 } else { 0 };
            ogg_page(&mut out, flags, granule, index as u32 + 2, &packet.data);
        }
        out
    }

    /// An IVF file of VP9 `packets`, `width` by `height`.
    fn ivf(width: u16, height: u16, packets: &[&MediaPacket]) -> Vec<u8> {
        let timescale = packets[0].timescale;
        let mut out = b"DKIF".to_vec();
        out.extend(0u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend(b"VP90");
        out.extend(width.to_le_bytes());
        out.extend(height.to_le_bytes());
        out.extend(timescale.to_le_bytes());
        out.extend(1u32.to_le_bytes());
        out.extend((packets.len() as u32).to_le_bytes());
        out.extend([0; 4]);
        for packet in packets {
            out.extend((packet.data.len() as u32).to_le_bytes());
            out.extend(packet.pts.to_le_bytes());
            out.extend(&packet.data);
        }
        out
    }

    /// The input arguments and bytes that give ffmpeg `packets` of fixture `name`, after
    /// `initialization`, in a form it reads.
    fn feed(
        name: &str,
        initialization: &[u8],
        packets: &[&MediaPacket],
    ) -> (Vec<&'static str>, Vec<u8>) {
        let concatenated = |header: &[u8]| {
            let mut out = header.to_vec();
            for packet in packets {
                out.extend(&packet.data);
            }
            out
        };
        match name {
            "h264" => (vec!["-f", "h264"], concatenated(initialization)),
            "hevc" | "hevc_open_gop" => (vec!["-f", "hevc"], concatenated(initialization)),
            // The configuration OBUs go after the first temporal delimiter.
            "av1" => {
                let mut stream = concatenated(&[]);
                let at = TEMPORAL_DELIMITER.len();
                stream.splice(at..at, initialization.iter().copied());
                (vec!["-f", "obu"], stream)
            }
            "flac" => (vec!["-f", "flac"], concatenated(initialization)),
            "vp9" => (vec!["-f", "ivf"], ivf(96, 64, packets)),
            "aac" | "he_aac" => (vec!["-f", "aac"], adts(initialization, packets)),
            "opus" => (vec!["-f", "ogg"], ogg_opus(initialization, packets)),
            "mp3" => (vec!["-f", "mp3"], concatenated(&[])),
            "ac3" => (vec!["-f", "ac3"], concatenated(&[])),
            "eac3" => (vec!["-f", "eac3"], concatenated(&[])),
            "pcm" => (
                vec!["-f", "s16le", "-ar", "8000", "-ac", "1"],
                concatenated(&[]),
            ),
            _ => unreachable!("{name}"),
        }
    }

    #[test]
    fn every_codec_decodes_to_the_source_output() {
        for name in VIDEO.iter().chain(&AUDIO) {
            let stream = media_stream(name);
            let initialization = stream.initialization().data.clone();
            let packets: Vec<MediaPacket> = stream.map(Result::unwrap).collect();
            let packets: Vec<&MediaPacket> = packets.iter().collect();
            let (input_args, input) = feed(name, &initialization, &packets);
            // Raw PCM is read back in frames of ffmpeg's own size, so it is hashed whole.
            let muxer = if *name == "pcm" { "hash" } else { "framehash" };
            let decoded = frame_hashes(&input_args, input, muxer);
            // Untrimmed, as the decoder-ready stream is: its trims are the packets'.
            let reference = frame_hashes(&["-ignore_editlist", "1"], source(name), muxer);
            assert_eq!(decoded, reference, "{name}");
        }
    }

    #[test]
    fn adts_decodes_to_the_source_output() {
        for name in ["aac", "he_aac"] {
            let stream = packets(name)
                .into_media_stream_as(DecoderFormat::AacAdts)
                .unwrap();
            let mut adts = stream.initialization().data.to_vec();
            for packet in stream {
                adts.extend(&packet.unwrap().data);
            }
            let decoded = frame_hashes(&["-f", "aac"], adts, "framehash");
            let reference = frame_hashes(&["-ignore_editlist", "1"], source(name), "framehash");
            assert_eq!(decoded, reference, "{name}");
        }
    }

    #[test]
    fn video_decodes_from_a_seek_to_any_frame_to_the_source_frames() {
        for name in VIDEO {
            let reference = frame_hashes(&[], source(name), "framehash");
            let all: Vec<MediaPacket> = media_stream(name).map(Result::unwrap).collect();
            let timescale = all[0].timescale;

            // An exact and a keyframe seek to every frame's presentation time.
            let targets = all.iter().filter(|packet| packet.pts >= 0);
            for (target, keyframe) in targets.flat_map(|target| [(target, false), (target, true)]) {
                let time = Duration::from_secs_f64(target.pts as f64 / f64::from(timescale));
                let context = format!("{name} seeking to {} keyframe {keyframe}", target.pts);
                let mut stream = media_stream(name);
                let initialization = stream.initialization().data.clone();
                let seeked = seek(&mut stream, time, keyframe);
                let rest: Vec<MediaPacket> = stream.map(Result::unwrap).collect();
                let fed: Vec<&MediaPacket> = seeked.preroll.iter().chain(&rest).collect();
                let (input_args, input) = feed(name, &initialization, &fed);
                let decoded = frame_hashes(&input_args, input, "framehash");

                // Decoding from the keyframe shows the last frames of the source, which
                // must include every frame from `discard_before` on. An open-GOP
                // keyframe's leading pictures reference the GOP before it, so they may not
                // be among them.
                assert!(fed[0].keyframe, "{context}");
                let shown = all
                    .iter()
                    .filter(|packet| !seeked.discards(packet.pts, packet.duration))
                    .count();
                assert!(
                    decoded.len() >= shown,
                    "{context}: {} frames decoded, {shown} to show",
                    decoded.len()
                );
                assert_eq!(
                    decoded,
                    reference[reference.len() - decoded.len()..],
                    "{context}"
                );
            }
        }
    }

    #[test]
    fn ogg_crc_is_crc_32_posix_without_its_final_xor() {
        // CRC-32/POSIX's check value, 0x765e7680, before its final XOR with all ones.
        assert_eq!(ogg_crc(b"123456789"), 0x765e_7680 ^ 0xffff_ffff);
    }
}
