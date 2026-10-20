//! End-to-end live-stream tests using a controllable clock and a local server
//! whose manifest can change between refreshes.

use super::clock::ManualClock;
use super::{SegmentCache, SegmentLocation, SegmentStream, Transport};
use crate::{BehindWindow, DashManifest, Error, LiveDelay, Retention};
use bytes::Bytes;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MPD_URL: &str = "https://cdn.test/live.mpd";

/// Local server with a replaceable MPD and segment bodies equal to their file names.
struct Server {
    mpd: Mutex<String>,
    mpd_fetches: AtomicUsize,
    /// Segment paths requested by the client, in request order.
    gets: Mutex<Vec<String>>,
}

impl Server {
    fn new(mpd: &str) -> Arc<Self> {
        Arc::new(Self {
            mpd: Mutex::new(mpd.to_owned()),
            mpd_fetches: AtomicUsize::new(0),
            gets: Mutex::default(),
        })
    }

    /// Returns a snapshot of segment paths requested so far.
    fn gets(&self) -> Vec<String> {
        self.gets.lock().unwrap().clone()
    }

    /// Replaces the MPD returned by subsequent requests.
    fn publish(&self, mpd: &str) {
        *self.mpd.lock().unwrap() = mpd.to_owned();
    }

    /// Returns the number of MPD requests received.
    fn mpd_fetches(&self) -> usize {
        self.mpd_fetches.load(Ordering::SeqCst)
    }
}

impl Transport for Server {
    fn get(&self, segment: &SegmentLocation, _size_hint: Option<u64>) -> Result<Bytes, Error> {
        if segment.url == MPD_URL {
            self.mpd_fetches.fetch_add(1, Ordering::SeqCst);
            return Ok(Bytes::from(self.mpd.lock().unwrap().clone()));
        }
        let name = segment.url.trim_start_matches("https://cdn.test/");
        self.gets.lock().unwrap().push(name.to_owned());
        Ok(Bytes::from(name.to_owned()))
    }
}

/// Builds a live audio/video MPD with a 20-second time-shift window.
fn live_mpd(attributes: &str, template: &str) -> String {
    format!(
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                availabilityStartTime="1970-01-01T00:00:00Z" timeShiftBufferDepth="PT20S"
                {attributes}>
             <Period start="PT0S">
               <AdaptationSet mimeType="audio/mp4">
                 <Representation id="audio" bandwidth="1">{template}</Representation>
               </AdaptationSet>
               <AdaptationSet mimeType="video/mp4">
                 <Representation id="video" bandwidth="1">{template}</Representation>
               </AdaptationSet>
             </Period>
           </MPD>"#
    )
}

/// Returns a template for two-second segments numbered from 1.
fn numbered() -> String {
    live_mpd(
        "",
        r#"<SegmentTemplate initialization="$RepresentationID$/init.mp4"
                            media="$RepresentationID$/$Number$.m4s" duration="2"/>"#,
    )
}

/// Returns a timeline template with two-second `$Time$` segments and a
/// ten-second manifest refresh interval.
fn timeline(published: u64, entries: &str) -> String {
    live_mpd(
        &format!(
            r#"minimumUpdatePeriod="PT10S" publishTime="1970-01-01T00:{:02}:{:02}Z""#,
            published / 60,
            published % 60
        ),
        &format!(
            r#"<SegmentTemplate initialization="$RepresentationID$/init.mp4"
                                media="$RepresentationID$/$Time$.m4s">
                 <SegmentTimeline>{entries}</SegmentTimeline>
               </SegmentTemplate>"#
        ),
    )
}

/// Parses `mpd` as if fetched from the test server at wall-clock second `secs`.
fn manifest(mpd: &str, clock: &Arc<ManualClock>, secs: u64) -> DashManifest {
    clock.set(secs);
    DashManifest::new(mpd)
        .unwrap()
        .with_base_url(MPD_URL)
        .unwrap()
        .with_clock(clock.clone())
}

/// Opens representation `id`, using `delay` or the MPD's suggested live delay.
fn open(
    manifest: &DashManifest,
    server: &Arc<Server>,
    id: &str,
    delay: Option<LiveDelay>,
) -> SegmentStream {
    let cache = Arc::new(SegmentCache::new(server.clone()));
    manifest
        .segment_stream_retained(id, cache, Retention::default(), delay)
        .unwrap()
}

/// Reads the next `count` segment bodies and decodes them as UTF-8 text.
fn take(stream: &mut SegmentStream, count: usize) -> Vec<String> {
    (0..count)
        .map(|_| {
            let body = stream.next().unwrap().unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        })
        .collect()
}

fn secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

#[test]
fn a_live_stream_starts_behind_the_edge_and_waits_for_new_segments() {
    let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
    let manifest = manifest(&numbered(), &clock, 100);
    assert!(manifest.is_live());

    // Four segments behind the edge at 100 s, by default.
    let mut stream = open(&manifest, &server, "audio", None);
    assert!(stream.is_live());
    assert_eq!(stream.live_edge(), Some(secs(100)));
    assert_eq!(stream.available(), Some(secs(80)..secs(100)));
    assert_eq!(
        take(&mut stream, 5),
        [
            "audio/init.mp4",
            "audio/47.m4s",
            "audio/48.m4s",
            "audio/49.m4s",
            "audio/50.m4s"
        ]
    );

    // The next segment once it has been produced.
    clock.set(102);
    stream.wake_worker();
    assert_eq!(take(&mut stream, 1), ["audio/51.m4s"]);
    assert_eq!(stream.live_edge(), Some(secs(102)));
    assert_eq!(
        server.mpd_fetches(),
        0,
        "without @minimumUpdatePeriod, never refetched"
    );
}

#[test]
fn a_chosen_delay_overrides_the_manifest() {
    let suggested = live_mpd(
        r#"suggestedPresentationDelay="PT10S""#,
        r#"<SegmentTemplate initialization="i.mp4" media="$Number$.m4s" duration="2"/>"#,
    );
    let (server, clock) = (Server::new(&suggested), ManualClock::at(0));
    let manifest = manifest(&suggested, &clock, 100);
    let first = |delay| {
        let mut stream = open(&manifest, &server, "audio", delay);
        take(&mut stream, 2).pop().unwrap()
    };
    assert_eq!(first(None), "46.m4s", "the manifest's ten seconds");
    assert_eq!(first(Some(LiveDelay::LiveEdge)), "50.m4s");
    assert_eq!(first(Some(LiveDelay::Time(secs(3)))), "49.m4s");
    assert_eq!(first(Some(LiveDelay::Segments(2))), "49.m4s");
    assert_eq!(
        first(Some(LiveDelay::Time(secs(60)))),
        "41.m4s",
        "no further back than the time-shift window"
    );
}

#[test]
fn live_segments_open_only_from_their_manifest() {
    let clock = ManualClock::at(0);
    let manifest = manifest(&numbered(), &clock, 100);
    let segments = manifest.segments("audio").unwrap();
    assert!(segments.is_live());
    assert_eq!(segments.end(), None);
    assert_eq!(segments.available_when(clock_time(100)), 41..51);
    assert!(matches!(
        SegmentStream::new(segments),
        Err(Error::DashManifestUnsupportedLive(_))
    ));
}

fn clock_time(secs: u64) -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn a_refreshed_timeline_carries_on_with_the_same_numbers() {
    let first = timeline(20, r#"<S t="0" d="2" r="9"/>"#);
    let (server, clock) = (Server::new(&first), ManualClock::at(0));
    let manifest = manifest(&first, &clock, 20);
    let mut stream = open(&manifest, &server, "audio", Some(LiveDelay::LiveEdge));
    assert_eq!(take(&mut stream, 2), ["audio/init.mp4", "audio/18.m4s"]);

    // The listed segments run out; a refresh lists more.
    server.publish(&timeline(22, r#"<S t="10" d="2" r="9"/>"#));
    clock.set(22);
    stream.wake_worker();
    assert_eq!(take(&mut stream, 1), ["audio/20.m4s"]);
    assert_eq!(server.mpd_fetches(), 1);
}

#[test]
fn every_stream_of_a_presentation_shares_one_refresh() {
    let first = timeline(20, r#"<S t="0" d="2" r="9"/>"#);
    let (server, clock) = (Server::new(&first), ManualClock::at(0));
    let manifest = manifest(&first, &clock, 20);
    let mut streams = ["audio", "video"].map(|id| {
        let mut stream = open(&manifest, &server, id, Some(LiveDelay::LiveEdge));
        take(&mut stream, 2);
        stream
    });

    server.publish(&timeline(22, r#"<S t="10" d="2" r="9"/>"#));
    clock.set(22);
    for stream in &streams {
        stream.wake_worker();
    }
    let [audio, video] = &mut streams;
    assert_eq!(take(audio, 1), ["audio/20.m4s"]);
    assert_eq!(take(video, 1), ["video/20.m4s"]);
    assert_eq!(server.mpd_fetches(), 1);
}

#[test]
fn a_manifest_turning_static_ends_the_stream() {
    let first = timeline(20, r#"<S t="0" d="2" r="9"/>"#);
    let (server, clock) = (Server::new(&first), ManualClock::at(0));
    let manifest = manifest(&first, &clock, 20);
    let mut stream = open(&manifest, &server, "audio", Some(LiveDelay::LiveEdge));
    take(&mut stream, 2);

    // The final manifest ends the presentation at 24 s.
    let ended = timeline(22, r#"<S t="10" d="2" r="6"/>"#).replace(
        r#"type="dynamic""#,
        r#"type="static" mediaPresentationDuration="PT24S""#,
    );
    server.publish(&ended);
    clock.set(22);
    stream.wake_worker();
    assert_eq!(take(&mut stream, 2), ["audio/20.m4s", "audio/22.m4s"]);
    assert!(stream.next().is_none());
    assert!(!stream.is_live());
}

#[test]
fn a_live_manifest_that_never_changes_ends_with_its_timeline() {
    let fixed =
        timeline(20, r#"<S t="0" d="2" r="9"/>"#).replace(r#"minimumUpdatePeriod="PT10S""#, "");
    let (server, clock) = (Server::new(&fixed), ManualClock::at(0));
    let manifest = manifest(&fixed, &clock, 20);
    let mut stream = open(&manifest, &server, "audio", Some(LiveDelay::LiveEdge));
    assert_eq!(take(&mut stream, 2), ["audio/init.mp4", "audio/18.m4s"]);
    assert!(stream.next().is_none());
    assert_eq!(server.mpd_fetches(), 0);
}

/// Opens `id` with the default four-segment live delay and the requested
/// behind-window `policy`.
fn open_behind(
    manifest: &DashManifest,
    cache: &Arc<SegmentCache>,
    id: &str,
    policy: BehindWindow,
) -> SegmentStream {
    manifest
        .segment_stream_retained(id, cache.clone(), Retention::default(), None)
        .unwrap()
        .with_behind_window(policy)
}

/// Creates a stream whose next segment expires after the clock jumps from
/// 100 to 200 seconds.
fn fallen_behind(policy: BehindWindow) -> (SegmentStream, Arc<Server>, Arc<ManualClock>) {
    let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
    let manifest = manifest(&numbered(), &clock, 100);
    let cache = Arc::new(SegmentCache::new(server.clone()));
    let mut stream = open_behind(&manifest, &cache, "audio", policy);
    assert_eq!(take(&mut stream, 5).last().unwrap(), "audio/50.m4s");
    clock.set(200);
    stream.wake_worker();
    (stream, server, clock)
}

#[test]
fn falling_behind_the_window_fails_until_a_seek_back_into_it() {
    let (mut stream, server, _clock) = fallen_behind(BehindWindow::Fail);
    assert!(matches!(
        stream.next(),
        Some(Err(Error::BehindLiveWindow { available })) if available == (secs(180)..secs(200))
    ));
    assert!(stream.next().is_none(), "the error ends the items");

    // A successful seek into the current window clears the iteration error.
    let position = stream.seek_time(secs(190)).unwrap();
    assert_eq!(position.time.unwrap().timestamp, secs(190));
    assert_eq!(position.byte, None, "the segments skipped have no sizes");
    assert_eq!(take(&mut stream, 2), ["audio/96.m4s", "audio/97.m4s"]);
    let expired = |name: &String| {
        let number = name.trim_start_matches("audio/").trim_end_matches(".m4s");
        number.parse::<u64>().is_ok_and(|number| (51..91).contains(&number))
    };
    assert!(!server.gets().iter().any(expired), "never fetches expired segments");
}

#[test]
fn reading_through_io_fails_with_the_error_inside() {
    use std::io::Read;

    let (mut stream, _server, _clock) = fallen_behind(BehindWindow::Fail);
    let error = stream.read(&mut [0; 16]).unwrap_err();
    let inner = error.get_ref().and_then(|inner| inner.downcast_ref::<Error>());
    assert!(matches!(inner, Some(Error::BehindLiveWindow { .. })));
}

#[test]
fn falling_behind_the_window_jumps_as_the_policy_says() {
    let (mut stream, _server, _clock) = fallen_behind(BehindWindow::WindowStart);
    assert_eq!(take(&mut stream, 2), ["audio/91.m4s", "audio/92.m4s"]);

    // LiveEdge resumes four two-second segments behind the new edge.
    let (mut stream, _server, _clock) = fallen_behind(BehindWindow::LiveEdge);
    assert_eq!(take(&mut stream, 2), ["audio/97.m4s", "audio/98.m4s"]);
}

#[test]
fn live_seeks_land_inside_the_window() {
    let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
    let manifest = manifest(&numbered(), &clock, 100);
    let cache = Arc::new(SegmentCache::new(server.clone()));
    let mut stream = open_behind(&manifest, &cache, "audio", BehindWindow::Fail);
    take(&mut stream, 1);
    let landed = |stream: &mut SegmentStream, time| {
        let position = stream.seek_time(time).unwrap();
        (position.byte, position.time.unwrap().timestamp)
    };

    // Clamp an old time to the window start and a future time to the live delay.
    assert_eq!(landed(&mut stream, secs(0)), (None, secs(80)));
    assert_eq!(take(&mut stream, 1), ["audio/41.m4s"]);
    assert_eq!(landed(&mut stream, secs(500)), (Some(14), secs(92)));
    assert_eq!(take(&mut stream, 1), ["audio/47.m4s"]);

    // A seek may reach available media before the stream's original join point.
    assert_eq!(landed(&mut stream, secs(85)), (None, secs(84)));
    assert_eq!(take(&mut stream, 2), ["audio/43.m4s", "audio/44.m4s"]);
}

#[test]
fn a_live_buffer_covers_the_moving_window() {
    let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
    let manifest = manifest(&numbered(), &clock, 100);
    let cache = Arc::new(SegmentCache::new(server.clone()));
    let mut stream = open_behind(&manifest, &cache, "audio", BehindWindow::Fail);
    let buffer = stream.buffer().unwrap();
    take(&mut stream, 5);
    let total = buffer.status().total;
    assert_eq!((total.segments, total.start, total.end), (10, secs(80), secs(100)));
    assert_eq!(buffer.status().ranges, [secs(92)..secs(100)]);

    // Moving ten seconds replaces five expired segments with five new ones.
    clock.set(110);
    stream.wake_worker();
    take(&mut stream, 5);
    let status = buffer.status();
    let total = status.total;
    assert_eq!((total.segments, total.start, total.end), (10, secs(90), secs(110)));
    assert_eq!(status.ranges, [secs(92)..secs(110)]);
}

#[test]
fn a_refresh_leaving_a_gap_is_handled_as_falling_behind() {
    let gapped = |policy| {
        let first = timeline(20, r#"<S t="0" d="2" r="9"/>"#);
        let (server, clock) = (Server::new(&first), ManualClock::at(0));
        let manifest = manifest(&first, &clock, 20);
        let cache = Arc::new(SegmentCache::new(server.clone()));
        let mut stream = manifest
            .segment_stream_retained("audio", cache, Retention::default(), Some(LiveDelay::LiveEdge))
            .unwrap()
            .with_behind_window(policy);
        assert_eq!(take(&mut stream, 2), ["audio/init.mp4", "audio/18.m4s"]);

        // The refreshed timeline starts at 24 s, leaving the expected 20 s segment absent.
        server.publish(&timeline(26, r#"<S t="24" d="2" r="9"/>"#));
        clock.set(26);
        stream.wake_worker();
        stream
    };
    assert_eq!(
        take(&mut gapped(BehindWindow::WindowStart), 1),
        ["audio/24.m4s"]
    );
    assert!(matches!(
        gapped(BehindWindow::Fail).next(),
        Some(Err(Error::BehindLiveWindow { available })) if available == (secs(24)..secs(26))
    ));
}

#[test]
fn a_long_live_stream_keeps_bounded_state() {
    let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
    let manifest = manifest(&numbered(), &clock, 100);
    let cache = Arc::new(SegmentCache::new(server.clone()));
    let mut stream = open_behind(&manifest, &cache, "audio", BehindWindow::Fail);
    take(&mut stream, 5);

    // Advance through ten minutes while consuming each two-second segment.
    for second in (102..700).step_by(2) {
        clock.set(second);
        stream.wake_worker();
        take(&mut stream, 1);

        // Scheduling and cache state stay close to the ten-segment live window.
        assert!(stream.scheduled_len() <= 12, "{}", stream.scheduled_len());
        assert!(cache.store.len() <= 13, "{}", cache.store.len());
    }
}

#[cfg(feature = "async")]
mod nonblocking {
    use super::*;
    use crate::nonblocking::segment::{SegmentCache, SegmentStream, Transport as AsyncTransport};
    use futures_util::StreamExt;
    use futures_util::future::BoxFuture;

    impl AsyncTransport for Server {
        fn get<'a>(
            &'a self,
            segment: &'a SegmentLocation,
            size_hint: Option<u64>,
        ) -> BoxFuture<'a, Result<Bytes, Error>> {
            Box::pin(async move { Transport::get(self, segment, size_hint) })
        }
    }

    async fn open(manifest: &DashManifest, server: &Arc<Server>, id: &str) -> SegmentStream {
        let manifest = crate::nonblocking::DashManifest::from(manifest.clone());
        let cache = Arc::new(SegmentCache::new(server.clone()));
        manifest
            .segment_stream_retained(id, cache, Retention::default(), Some(LiveDelay::LiveEdge))
            .await
            .unwrap()
    }

    async fn take(stream: &mut SegmentStream, count: usize) -> Vec<String> {
        let mut bodies = Vec::new();
        for _ in 0..count {
            let body = stream.next().await.unwrap().unwrap();
            bodies.push(String::from_utf8(body.to_vec()).unwrap());
        }
        bodies
    }

    #[tokio::test]
    async fn falling_behind_the_window_fails_or_jumps() {
        let fallen_behind = async |policy| {
            let (server, clock) = (Server::new(&numbered()), ManualClock::at(0));
            let manifest = manifest(&numbered(), &clock, 100);
            let mut stream = open(&manifest, &server, "audio")
                .await
                .with_behind_window(policy);
            assert_eq!(
                take(&mut stream, 2).await,
                ["audio/init.mp4", "audio/50.m4s"]
            );
            clock.set(200);
            stream.wake_worker();
            stream
        };

        // WindowStart resumes at the earliest available segment.
        let mut stream = fallen_behind(BehindWindow::WindowStart).await;
        assert_eq!(take(&mut stream, 1).await, ["audio/91.m4s"]);

        // Fail ends iteration until a successful seek re-enters the window.
        let mut stream = fallen_behind(BehindWindow::Fail).await;
        assert!(matches!(
            stream.next().await,
            Some(Err(Error::BehindLiveWindow { available })) if available == (secs(180)..secs(200))
        ));
        assert!(stream.next().await.is_none());
        stream.seek_time(secs(190)).await.unwrap();
        assert_eq!(take(&mut stream, 1).await, ["audio/96.m4s"]);
    }

    #[tokio::test]
    async fn a_live_stream_refreshes_and_waits_for_new_segments() {
        let first = timeline(20, r#"<S t="0" d="2" r="9"/>"#);
        let (server, clock) = (Server::new(&first), ManualClock::at(0));
        let manifest = manifest(&first, &clock, 20);
        let mut stream = open(&manifest, &server, "audio").await;
        assert!(stream.is_live());
        assert_eq!(
            take(&mut stream, 2).await,
            ["audio/init.mp4", "audio/18.m4s"]
        );

        server.publish(&timeline(22, r#"<S t="10" d="2" r="9"/>"#));
        clock.set(22);
        stream.wake_worker();
        assert_eq!(take(&mut stream, 1).await, ["audio/20.m4s"]);
        assert_eq!(server.mpd_fetches(), 1);
    }
}
