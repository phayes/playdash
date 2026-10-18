//! `PacketStream` against real DASH output from ffmpeg: every codec it identifies, its
//! packets compared with ffprobe's, and where seeks land.
#![cfg(feature = "sync")]

mod common;

use common::{BASE, Fixtures, Probe, Probed, assert_matches, fixture, muxed, segment_stream};
use playdash::packet::{Packet, PacketSeeked, PacketStream};
use playdash::segment::SegmentStream;
use playdash::{Codec, ContentKind, DashManifest, Error, Retention, SegmentCache};
use std::sync::Arc;
use std::time::Duration;

fn packets(name: &str) -> PacketStream {
    PacketStream::new(segment_stream(name, Arc::default())).unwrap()
}

fn probed(name: &str) -> Vec<Probed> {
    serde_json::from_slice::<Probe>(&fixture(&format!("packets/{name}.json")))
        .unwrap()
        .packets
}

const AUDIO: [&str; 8] = ["aac", "he_aac", "opus", "flac", "ac3", "eac3", "mp3", "pcm"];
const VIDEO: [&str; 5] = ["h264", "hevc", "hevc_open_gop", "av1", "vp9"];

#[test]
fn every_codec_is_identified() {
    let audio = |name: &str| match packets(name).codec().clone() {
        Codec::Aac(aac) => (
            aac.audio.sample_rate,
            aac.audio.channels,
            format!("aac {} {}", aac.object_type, aac.sbr),
        ),
        Codec::Opus(opus) => (
            opus.audio.sample_rate,
            opus.audio.channels,
            format!("opus {}", opus.pre_skip),
        ),
        Codec::Pcm(pcm) => (
            pcm.audio.sample_rate,
            pcm.audio.channels,
            format!("pcm {} {}", pcm.bits_per_sample, pcm.little_endian),
        ),
        Codec::Flac(audio) => (audio.sample_rate, audio.channels, "flac".to_owned()),
        Codec::Ac3(audio) => (audio.sample_rate, audio.channels, "ac3".to_owned()),
        Codec::Eac3(audio) => (audio.sample_rate, audio.channels, "eac3".to_owned()),
        Codec::Mp3(audio) => (audio.sample_rate, audio.channels, "mp3".to_owned()),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(audio("aac"), (48_000, 1, "aac 2 false".to_owned()));
    assert_eq!(audio("he_aac"), (48_000, 1, "aac 2 true".to_owned()));
    assert_eq!(audio("opus"), (48_000, 1, "opus 312".to_owned()));
    assert_eq!(audio("flac"), (22_050, 1, "flac".to_owned()));
    assert_eq!(audio("ac3"), (48_000, 1, "ac3".to_owned()));
    assert_eq!(audio("eac3"), (48_000, 1, "eac3".to_owned()));
    assert_eq!(audio("mp3"), (48_000, 1, "mp3".to_owned()));
    assert_eq!(audio("pcm"), (8_000, 1, "pcm 16 true".to_owned()));

    for name in VIDEO {
        let codec = packets(name).codec().clone();
        let video = match (&codec, name) {
            (Codec::H264(video), "h264")
            | (Codec::Hevc(video), "hevc" | "hevc_open_gop")
            | (Codec::Av1(video), "av1")
            | (Codec::Vp9(video), "vp9") => video.clone(),
            _ => panic!("{name}: {codec:?}"),
        };
        assert_eq!((video.width, video.height), (96, 64), "{name}");
        assert!(!video.config.is_empty(), "{name}");
    }
}

/// Checks `packets` are every packet of fixture `name`.
fn check_packets(name: &str, packets: &[Packet], audio: bool) {
    common::check_packets(name, packets, &probed(name), audio);
}

#[test]
fn packets_match_ffprobe() {
    for (names, audio) in [(&AUDIO[..], true), (&VIDEO[..], false)] {
        for &name in names {
            let packets: Vec<Packet> = packets(name).map(Result::unwrap).collect();
            check_packets(name, &packets, audio);
        }
    }
}

/// The index of the packet presenting at `ticks`.
fn presenting(expected: &[Probed], ticks: i64) -> usize {
    expected
        .iter()
        .position(|packet| {
            let duration = packet.duration.expect("audio packets have durations");
            packet.pts <= ticks && ticks < packet.pts + duration as i64
        })
        .expect("a packet presents at the time")
}

/// `time` in ticks of `timescale`, rounded to the nearest as seeks round it.
fn ticks(time: Duration, timescale: u32) -> i64 {
    ((time.as_nanos() * u128::from(timescale) + 500_000_000) / 1_000_000_000) as i64
}

/// Where audio seeks go.
const AUDIO_SEEK: Duration = Duration::from_millis(700);

/// Checks a packet-accurate seek of fixture `name` to [`AUDIO_SEEK`], on a stream of
/// `timescale` decoding `preroll` first, returned `seeked` and then the packets `rest`.
fn check_audio_seek(
    name: &str,
    timescale: u32,
    preroll: Duration,
    seeked: &PacketSeeked,
    rest: &[Packet],
) {
    let ticks = ticks(AUDIO_SEEK, timescale);
    let preroll = self::ticks(preroll, timescale);
    let expected = probed(name);
    let at = presenting(&expected, ticks);

    assert_eq!(seeked.requested, AUDIO_SEEK, "{name}");
    assert_eq!(
        seeked.landed, expected[at].pts,
        "{name} lands on the packet at the time"
    );
    assert_eq!(seeked.discard_before, ticks, "{name}");
    // Pre-roll: the packets just before, covering at least the codec's pre-roll.
    let first = at - seeked.preroll.len();
    for (offset, packet) in seeked.preroll.iter().enumerate() {
        assert_matches(name, first + offset, packet, &expected, true);
    }
    assert!(
        expected[first].pts <= ticks - preroll,
        "{name} pre-roll covers {preroll} ticks"
    );
    if let Some(later) = first.checked_add(1).filter(|&later| later <= at) {
        assert!(
            expected[later].pts > ticks - preroll,
            "{name} pre-roll is no longer than needed"
        );
    }
    // Then the stream carries on from the landing packet.
    assert_eq!(rest.len(), expected.len() - at, "{name}");
    for (offset, packet) in rest.iter().enumerate() {
        assert_matches(name, at + offset, packet, &expected, true);
    }
}

#[test]
fn audio_seeks_land_on_the_packet_at_the_time_after_its_preroll() {
    for name in AUDIO {
        let mut stream = packets(name);
        let (timescale, preroll) = (stream.timescale(), stream.preroll());
        let seeked = stream.seek_time(AUDIO_SEEK).unwrap();
        let rest: Vec<Packet> = stream.map(Result::unwrap).collect();
        check_audio_seek(name, timescale, preroll, &seeked, &rest);
    }
}

/// Checks a keyframe seek of fixture `name` to [`AUDIO_SEEK`], on a stream of
/// `timescale` decoding `preroll` first, returned `seeked` and then the packets `rest`.
/// Every packet of the audio fixtures is a keyframe.
fn check_audio_keyframe_seek(
    name: &str,
    timescale: u32,
    preroll: Duration,
    seeked: &PacketSeeked,
    rest: &[Packet],
) {
    let ticks = ticks(AUDIO_SEEK, timescale);
    let preroll = self::ticks(preroll, timescale);
    let expected = probed(name);
    // Decoding starts at the last packet a pre-roll before the time, and lands a
    // pre-roll after it.
    let first = expected
        .iter()
        .rposition(|packet| packet.pts <= ticks - preroll)
        .unwrap();
    let at = presenting(&expected, expected[first].pts + preroll);

    assert_eq!(seeked.requested, AUDIO_SEEK, "{name}");
    assert_eq!(
        (seeked.landed, seeked.discard_before),
        (expected[at].pts, expected[at].pts),
        "{name}"
    );
    assert!(seeked.landed <= ticks, "{name} lands at or before the time");
    assert_eq!(seeked.preroll.len(), at - first, "{name}");
    for (offset, packet) in seeked.preroll.iter().enumerate() {
        assert_matches(name, first + offset, packet, &expected, true);
    }
    assert_eq!(rest.len(), expected.len() - at, "{name}");
    for (offset, packet) in rest.iter().enumerate() {
        assert_matches(name, at + offset, packet, &expected, true);
    }
}

#[test]
fn audio_keyframe_seeks_land_a_preroll_after_the_packet_decoding_starts_at() {
    for name in AUDIO {
        let mut stream = packets(name);
        let (timescale, preroll) = (stream.timescale(), stream.preroll());
        let seeked = stream.seek_keyframe(AUDIO_SEEK).unwrap();
        let rest: Vec<Packet> = stream.map(Result::unwrap).collect();
        check_audio_keyframe_seek(name, timescale, preroll, &seeked, &rest);
    }
}

/// Where video seeks go.
const VIDEO_SEEK: Duration = Duration::from_millis(1_300);

/// Whether `packet` of fixture `name` is an HEVC RASL picture: its first slice's NAL unit
/// type is 8 or 9. The HEVC fixtures have four-byte NAL unit lengths.
fn is_rasl(name: &str, packet: &Packet) -> bool {
    if !name.starts_with("hevc") {
        return false;
    }
    let mut pos = 0;
    while pos + 5 <= packet.data.len() {
        let len = u32::from_be_bytes(packet.data[pos..pos + 4].try_into().unwrap()) as usize;
        let nal_type = packet.data[pos + 4] >> 1 & 0x3f;
        if nal_type <= 31 {
            return matches!(nal_type, 8 | 9);
        }
        pos += 4 + len;
    }
    false
}

/// Checks a packet-accurate seek of fixture `name` to [`VIDEO_SEEK`], on a stream of
/// `timescale`, returned `seeked` and then the packets `rest`.
fn check_video_seek(name: &str, timescale: u32, seeked: &PacketSeeked, rest: &[Packet]) {
    let ticks = ticks(VIDEO_SEEK, timescale);
    let expected = probed(name);
    let keyframe = expected
        .iter()
        .rposition(|packet| packet.flags.starts_with('K') && packet.pts <= ticks)
        .unwrap();

    // Pre-roll and then the stream: every packet from that keyframe on, in order, but for
    // the keyframe's RASL pictures, which cannot be decoded from it.
    let all: Vec<Packet> = packets(name).map(Result::unwrap).collect();
    let next_keyframe = (keyframe + 1..expected.len())
        .find(|&index| expected[index].flags.starts_with('K'))
        .unwrap_or(expected.len());
    let kept: Vec<usize> = (keyframe..expected.len())
        .filter(|&index| !(index < next_keyframe && is_rasl(name, &all[index])))
        .collect();
    let decoded: Vec<&Packet> = seeked.preroll.iter().chain(rest).collect();
    assert_eq!(decoded.len(), kept.len(), "{name}");
    for (packet, &index) in decoded.iter().zip(&kept) {
        assert_matches(name, index, packet, &expected, false);
    }
    assert!(
        seeked.preroll.iter().all(|packet| packet.pts < ticks),
        "{name}"
    );
    assert_eq!(rest[0].pts, seeked.landed, "{name}");
    assert!(rest[0].pts + rest[0].duration as i64 > ticks, "{name}");
    assert!(
        rest.iter()
            .any(|packet| packet.pts <= ticks && ticks < packet.pts + packet.duration as i64),
        "{name}: the frame showing at the time is decoded after landing"
    );
    assert_eq!(seeked.discard_before, ticks, "{name}");
}

#[test]
fn video_seeks_decode_from_the_keyframe_before_the_time() {
    for name in VIDEO {
        let mut stream = packets(name);
        let timescale = stream.timescale();
        let seeked = stream.seek_time(VIDEO_SEEK).unwrap();
        let rest: Vec<Packet> = stream.map(Result::unwrap).collect();
        check_video_seek(name, timescale, &seeked, &rest);
    }
}

/// Checks a keyframe seek of fixture `name` to [`VIDEO_SEEK`], on a stream of
/// `timescale`, returned `seeked` and then the packets `rest`.
fn check_video_keyframe_seek(name: &str, timescale: u32, seeked: &PacketSeeked, rest: &[Packet]) {
    let ticks = ticks(VIDEO_SEEK, timescale);
    let expected = probed(name);
    let keyframe = expected
        .iter()
        .rposition(|packet| packet.flags.starts_with('K') && packet.pts <= ticks)
        .unwrap();

    assert_eq!(seeked.requested, VIDEO_SEEK, "{name}");
    let pts = expected[keyframe].pts;
    assert_eq!((seeked.landed, seeked.discard_before), (pts, pts), "{name}");
    // Nothing to pre-roll, unless the keyframe has leading pictures it would show: then
    // decoding starts at an earlier keyframe, all of whose output is before it.
    if let Some(first) = seeked.preroll.first() {
        assert!(first.keyframe, "{name}");
        assert!(
            seeked.preroll.iter().all(|packet| packet.pts < pts),
            "{name}"
        );
    }
    // The stream carries on from the keyframe, but for its RASL pictures when decoding
    // starts there.
    assert!(rest[0].keyframe, "{name}");
    assert_matches(name, keyframe, &rest[0], &expected, false);
    if seeked.preroll.is_empty() {
        assert!(
            rest.iter()
                .all(|packet| !is_rasl(name, packet) || packet.pts > pts),
            "{name}"
        );
    }
}

#[test]
fn video_keyframe_seeks_land_on_the_keyframe_before_the_time() {
    for name in VIDEO {
        let mut stream = packets(name);
        let timescale = stream.timescale();
        let seeked = stream.seek_keyframe(VIDEO_SEEK).unwrap();
        let rest: Vec<Packet> = stream.map(Result::unwrap).collect();
        check_video_keyframe_seek(name, timescale, &seeked, &rest);
    }
}

#[test]
fn seeks_land_on_the_next_packet_even_past_the_timelines_segment() {
    // The single-file fixtures' SegmentList @duration only approximates their segments, so
    // some times fall after the last packet of the segment the timeline names.
    for name in AUDIO.iter().chain(&VIDEO) {
        let first_pts = packets(name).next().unwrap().unwrap().pts;
        for millis in (0..1_500).step_by(20) {
            for keyframe in [false, true] {
                let context = format!("{name} {millis}ms keyframe {keyframe}");
                let time = Duration::from_millis(millis);
                let mut stream = packets(name);
                let seeked = match keyframe {
                    false => stream.seek_time(time),
                    true => stream.seek_keyframe(time),
                }
                .unwrap();
                let next = stream.next().expect(&context).unwrap();
                assert_eq!(seeked.landed, next.pts, "{context}");
                // At or before the time, unless that is before the first packet.
                if keyframe {
                    let ticks = ticks(time, stream.timescale()).max(first_pts);
                    assert!(
                        seeked.landed <= ticks,
                        "{context} lands at or before the time"
                    );
                }
            }
        }
    }
}

#[test]
fn keyframe_seeks_land_before_the_time_and_exact_seeks_reach_back() {
    let transport = Arc::new(Fixtures::default());
    let stream = segment_stream("aac", transport.clone()).with_retention(Retention {
        behind: Some(Duration::ZERO),
        ahead: Some(Duration::ZERO),
    });
    let mut stream = PacketStream::new(stream).unwrap();
    stream.next().unwrap().unwrap();
    assert_eq!(transport.gets_of("aac_1.m4s"), 1);

    // 600ms is in the second segment, which starts at 23552: one frame of pre-roll from
    // the frame at 27648 lands on the next, before the time.
    let expected = probed("aac");
    let second = expected
        .iter()
        .position(|packet| packet.pts >= 23_552)
        .unwrap();
    let decode_from = expected
        .iter()
        .position(|packet| packet.pts == 27_648)
        .unwrap();
    let seeked = stream.seek_keyframe(Duration::from_millis(600)).unwrap();
    assert_eq!(seeked.landed, expected[decode_from + 1].pts);
    assert!(seeked.landed < 28_800);
    assert_eq!(seeked.discard_before, seeked.landed);
    assert_eq!(seeked.preroll.len(), 1);
    assert_eq!(seeked.preroll[0].pts, expected[decode_from].pts);
    assert_eq!(
        transport.gets_of("aac_1.m4s"),
        1,
        "the first segment is not fetched again"
    );
    assert_eq!(stream.next().unwrap().unwrap().pts, seeked.landed);

    // An exact seek to the start of the second segment needs the first's last frame.
    let start = Duration::from_secs_f64(expected[second].pts as f64 / 48_000.0);
    let seeked = stream.seek_time(start).unwrap();
    assert_eq!(seeked.landed, expected[second].pts);
    assert_eq!(seeked.preroll.len(), 1);
    assert_eq!(seeked.preroll[0].pts, expected[second - 1].pts);
    assert_eq!(
        transport.gets_of("aac_1.m4s"),
        2,
        "fetched again, outside the window"
    );
}

#[test]
fn a_read_stream_or_encrypted_samples_are_refused() {
    let mut stream = segment_stream("aac", Arc::default());
    stream.next().unwrap().unwrap();
    assert!(matches!(
        PacketStream::new(stream),
        Err(Error::Initialization(_))
    ));

    // Shaka's cenc fixture, without keys.
    let xml = String::from_utf8(fixture("encryption/shaka_cenc.mpd")).unwrap();
    let manifest = DashManifest::new(xml)
        .unwrap()
        .with_base_url(&format!("{BASE}encryption/shaka_cenc.mpd"))
        .unwrap();
    let stream = manifest
        .segment_stream_with_cache(
            "0",
            Arc::new(SegmentCache::new(Arc::new(Fixtures::default()))),
        )
        .unwrap();
    assert!(matches!(
        PacketStream::new(stream),
        Err(Error::UnsupportedMedia { reason, .. }) if reason.contains("encrypted")
    ));
}

#[test]
fn muxed_tracks_are_listed_and_read_one_stream_each() {
    let transport = Arc::new(Fixtures::default());
    let cache = Arc::new(SegmentCache::new(transport.clone()));
    let segments = muxed();
    let media = segments.media_len();
    let stream = || SegmentStream::new_with_cache(segments.clone(), cache.clone()).unwrap();

    // The tracks, read from the initialization segment the stream already holds.
    let video_stream = stream();
    let tracks = video_stream.tracks().unwrap();
    let listed: Vec<_> = tracks.iter().map(|track| (track.id, track.kind)).collect();
    assert_eq!(listed, [(1, ContentKind::Video), (2, ContentKind::Audio)]);
    assert!(matches!(tracks[0].codec, Codec::H264(_)));
    assert!(matches!(tracks[1].codec, Codec::Aac(_)));
    assert_eq!(tracks[1].timescale, 48_000);

    // Reading needs a track chosen.
    assert!(matches!(
        PacketStream::new(stream()),
        Err(Error::UnsupportedMedia { reason, .. }) if reason.contains("2 tracks")
    ));
    assert!(matches!(
        PacketStream::with_track(stream(), 9),
        Err(Error::Initialization(_))
    ));

    // Each stream reads its own track's packets.
    let video = PacketStream::with_track(video_stream, 1).unwrap();
    let audio = PacketStream::with_track(stream(), 2).unwrap();
    assert_eq!((video.track_id(), audio.track_id()), (1, 2));
    check_packets(
        "muxed_v",
        &video.map(Result::unwrap).collect::<Vec<_>>(),
        false,
    );
    check_packets(
        "muxed_a",
        &audio.map(Result::unwrap).collect::<Vec<_>>(),
        true,
    );

    // Sharing a cache, every segment downloaded once.
    assert_eq!(transport.gets.lock().unwrap().len(), 1 + media);
}

#[test]
#[cfg(feature = "encryption")]
fn decrypted_packets_are_the_plain_packets() {
    use common::split_segments;
    use playdash::{Bytes, ContentKeys};

    let mut keys = ContentKeys::new();
    keys.insert_hex(
        "0123456789abcdef0123456789abcdef",
        "00112233445566778899aabbccddeeff",
    )
    .unwrap();
    let cache =
        || Arc::new(SegmentCache::new(Arc::new(Fixtures::default())).with_keys(keys.clone()));
    let data = |stream: SegmentStream| -> Vec<Bytes> {
        PacketStream::new(stream)
            .unwrap()
            .map(|packet| packet.unwrap().data)
            .collect()
    };

    let url = format!("{BASE}encryption/plain.mp4");
    let segments = split_segments(&url, &fixture("encryption/plain.mp4"));
    let plain = data(SegmentStream::new_with_cache(segments, cache()).unwrap());
    assert!(plain.len() > 10);

    for name in ["shaka_cenc", "shaka_cbcs"] {
        let xml = String::from_utf8(fixture(&format!("encryption/{name}.mpd"))).unwrap();
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_base_url(&format!("{BASE}encryption/{name}.mpd"))
            .unwrap();
        assert_eq!(
            data(manifest.segment_stream_with_cache("0", cache()).unwrap()),
            plain,
            "{name}"
        );
    }
    let url = format!("{BASE}encryption/ffmpeg_cenc.mp4");
    let segments = split_segments(&url, &fixture("encryption/ffmpeg_cenc.mp4"));
    assert_eq!(
        data(SegmentStream::new_with_cache(segments, cache()).unwrap()),
        plain,
        "ffmpeg_cenc"
    );
}

/// The same packets and seeks through `nonblocking::packet::PacketStream`.
#[cfg(feature = "async")]
mod nonblocking {
    use super::*;
    use common::async_segment_stream;
    use futures_util::StreamExt;
    use playdash::nonblocking;

    async fn packets(name: &str) -> nonblocking::packet::PacketStream {
        nonblocking::packet::PacketStream::new(async_segment_stream(name).await)
            .await
            .unwrap()
    }

    async fn collect(stream: nonblocking::packet::PacketStream) -> Vec<Packet> {
        stream.map(Result::unwrap).collect().await
    }

    #[tokio::test]
    async fn packets_match_ffprobe() {
        for (names, audio) in [(&AUDIO[..], true), (&VIDEO[..], false)] {
            for &name in names {
                check_packets(name, &collect(packets(name).await).await, audio);
            }
        }
    }

    #[tokio::test]
    async fn seeks_land_as_blocking_ones_do() {
        for name in AUDIO {
            let mut stream = packets(name).await;
            let (timescale, preroll) = (stream.timescale(), stream.preroll());
            let seeked = stream.seek_time(AUDIO_SEEK).await.unwrap();
            check_audio_seek(name, timescale, preroll, &seeked, &collect(stream).await);

            let mut stream = packets(name).await;
            let seeked = stream.seek_keyframe(AUDIO_SEEK).await.unwrap();
            let rest = collect(stream).await;
            check_audio_keyframe_seek(name, timescale, preroll, &seeked, &rest);
        }
        for name in VIDEO {
            let mut stream = packets(name).await;
            let timescale = stream.timescale();
            let seeked = stream.seek_time(VIDEO_SEEK).await.unwrap();
            check_video_seek(name, timescale, &seeked, &collect(stream).await);

            let mut stream = packets(name).await;
            let seeked = stream.seek_keyframe(VIDEO_SEEK).await.unwrap();
            check_video_keyframe_seek(name, timescale, &seeked, &collect(stream).await);
        }
    }

    #[tokio::test]
    async fn muxed_tracks_match_blocking_ones() {
        let cache = Arc::new(nonblocking::segment::SegmentCache::new(Arc::new(
            Fixtures::default(),
        )));
        let stream = || nonblocking::segment::SegmentStream::new_with_cache(muxed(), cache.clone());
        let blocking = SegmentStream::new_with_cache(
            muxed(),
            Arc::new(SegmentCache::new(Arc::new(Fixtures::default()))),
        )
        .unwrap();

        let video_stream = stream().await.unwrap();
        assert_eq!(video_stream.tracks().unwrap(), blocking.tracks().unwrap());
        let video = nonblocking::packet::PacketStream::with_track(video_stream, 1)
            .await
            .unwrap();
        let audio = nonblocking::packet::PacketStream::with_track(stream().await.unwrap(), 2)
            .await
            .unwrap();
        check_packets("muxed_v", &collect(video).await, false);
        check_packets("muxed_a", &collect(audio).await, true);
    }

    #[tokio::test]
    async fn codecs_and_keyframe_seeks_match_blocking_ones() {
        for name in AUDIO.iter().chain(&VIDEO) {
            let mut stream = packets(name).await;
            let mut blocking = super::packets(name);
            assert_eq!(stream.codec(), blocking.codec(), "{name}");
            assert_eq!(stream.preroll(), blocking.preroll(), "{name}");
            let time = Duration::from_millis(600);
            assert_eq!(
                stream.seek_keyframe(time).await.unwrap(),
                blocking.seek_keyframe(time).unwrap(),
                "{name}"
            );
            assert_eq!(
                collect(stream).await,
                blocking.map(Result::unwrap).collect::<Vec<_>>(),
                "{name}"
            );
        }
    }
}
