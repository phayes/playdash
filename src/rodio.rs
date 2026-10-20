//! Rodio source support for progressive MPEG-DASH playback.

#[cfg(feature = "encryption")]
use crate::ContentKeys;
use crate::segment::{SegmentStream, StreamHandle};
use crate::{BehindWindow, BufferHandle, DashManifest, Error, SegmentCache};
use ::rodio::{Decoder, Source, source::SeekError};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// A seekable rodio [`Source`] for an MPEG-DASH representation.
///
/// Use [`DashSource::new`] for unprotected media. Common-encrypted media needs
/// the `encryption` feature and [`DashSource::new_with_keys`].
///
/// Live playback starts at the manifest's suggested delay behind the live
/// edge, or four segments behind when no delay is specified, and waits for new
/// media at the edge. [`Source::total_duration`] returns `None`. Rodio seek
/// positions are relative to [`Self::start`], not presentation time zero.
/// Seeks are clamped to the current live window. By default, playback that
/// falls behind resumes at the window start; see [`Self::with_behind_window`].
///
/// # Examples
///
/// ```no_run
/// use playdash::{DashManifest, DashSource};
/// use rodio::{DeviceSinkBuilder, Player};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
/// let source = DashSource::new(&manifest, "audio")?;
///
/// let device = DeviceSinkBuilder::open_default_sink()?;
/// let player = Player::connect_new(device.mixer());
/// player.append(source);
/// player.sleep_until_end();
/// # Ok(())
/// # }
/// ```
pub struct DashSource {
    manifest: DashManifest,
    id: String,
    cache: Arc<SegmentCache>,
    mime_type: String,
    /// Recovery policy used after playback falls behind the live window.
    behind: BehindWindow,
    /// Presentation time corresponding to rodio position zero.
    start: Duration,
    /// End of static media in presentation time, or `None` while live.
    end: Option<Duration>,
    /// Buffer report shared across streams rebuilt by seeks.
    buffer: BufferHandle,
    /// Handle to the segment stream currently owned by `decoder`.
    stream: StreamHandle,
    decoder: Decoder<SegmentStream>,
}

impl DashSource {
    /// Opens a representation from `manifest` for progressive rodio playback.
    ///
    /// `id` uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`]. Fetches through a new
    /// [`SegmentCache`] dedicated to this source.
    pub fn new(manifest: &DashManifest, id: impl AsRef<str>) -> Result<Self, Error> {
        Self::new_with_cache(manifest, id, Arc::default())
    }

    /// Like [`DashSource::new`], but decrypts `cenc` or `cbcs` Common
    /// Encryption with `keys`.
    ///
    /// Fails with [`Error::MissingContentKey`] if the initialization segment
    /// names a KID that `keys` lacks. Unprotected representations play as
    /// with [`DashSource::new`].
    #[cfg(feature = "encryption")]
    pub fn new_with_keys(
        manifest: &DashManifest,
        id: impl AsRef<str>,
        keys: ContentKeys,
    ) -> Result<Self, Error> {
        let cache = SegmentCache::default().with_keys(keys);
        Self::new_with_cache(manifest, id, Arc::new(cache))
    }

    /// Like [`DashSource::new`], but fetches through `cache`, for a custom
    /// [`crate::Transport`], content keys, or to share connections and
    /// downloaded segments with other sources. A protected representation
    /// is rejected unless `cache` decrypts.
    pub fn new_with_cache(
        manifest: &DashManifest,
        id: impl AsRef<str>,
        cache: Arc<SegmentCache>,
    ) -> Result<Self, Error> {
        let id = id.as_ref();
        if let Some(scheme) = manifest.protection_scheme(id)?
            && !cache.decrypts()
        {
            return Err(Error::RodioProtectedContent(scheme));
        }
        let mime_type = manifest.mime_type(id)?.unwrap_or_default().to_owned();

        // Open the initial stream and establish rodio's presentation-time origin.
        let behind = BehindWindow::WindowStart;
        let stream = manifest
            .segment_stream_with_cache(id, cache.clone())?
            .with_behind_window(behind);
        let no_media = || Error::Initialization("rodio playback requires media segments".to_owned());
        let start = stream.join_time().ok_or_else(no_media)?;
        let end = match manifest.is_live() {
            true => None,
            false => Some(manifest.segments(id)?.end().ok_or_else(no_media)?),
        };

        // Keep one buffer handle subscribed across decoder rebuilds.
        let (stream, decoder) = Self::decoder(stream, &mime_type)?;
        let buffer = BufferHandle::empty(&cache.store);
        stream.subscribe_buffer(buffer.feed());
        Ok(Self {
            manifest: manifest.clone(),
            id: id.to_owned(),
            cache,
            mime_type,
            behind,
            start,
            end,
            buffer,
            stream,
            decoder,
        })
    }

    /// Sets how playback recovers after falling behind the live window.
    ///
    /// The default, [`BehindWindow::WindowStart`], resumes at the earliest
    /// available media. [`BehindWindow::Fail`] ends this rodio source because
    /// [`Source`] cannot report playback errors. This setting has no effect on
    /// static presentations.
    pub fn with_behind_window(mut self, policy: BehindWindow) -> Self {
        self.behind = policy;
        self.stream.set_behind_window(policy);
        self
    }

    /// Returns a handle reporting how much currently relevant media is cached.
    ///
    /// Obtain the handle before moving this source into rodio. It remains
    /// usable from other threads, follows seeks, and does not affect playback.
    pub fn buffer(&self) -> BufferHandle {
        self.buffer.clone()
    }

    /// Returns the presentation time corresponding to rodio position zero.
    pub fn start(&self) -> Duration {
        self.start
    }

    /// Returns the currently seekable presentation-time range.
    ///
    /// For static media this is the full presentation. Returns `None` for
    /// untimed media or while a live presentation has no available segment.
    pub fn available(&self) -> Option<Range<Duration>> {
        self.stream.available()
    }

    /// Returns the current live edge, or `None` for static media or an empty
    /// live window.
    pub fn live_edge(&self) -> Option<Duration> {
        self.stream.live_edge()
    }

    /// Builds a decoder and returns weak access to its segment stream.
    fn decoder(
        stream: SegmentStream,
        mime_type: &str,
    ) -> Result<(StreamHandle, Decoder<SegmentStream>), Error> {
        // The decoder is unseekable and seeks rebuild by timeline, so byte
        // sizes from eager HEADs would go unused.
        let handle = stream.handle();
        let decoder = Decoder::builder()
            .with_data(stream)
            .with_mime_type(mime_type)
            .build()?;
        Ok((handle, decoder))
    }
}

impl Iterator for DashSource {
    type Item = ::rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        self.decoder.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.decoder.size_hint()
    }
}

impl Source for DashSource {
    fn current_span_len(&self) -> Option<usize> {
        self.decoder.current_span_len()
    }

    fn channels(&self) -> ::rodio::ChannelCount {
        self.decoder.channels()
    }

    fn sample_rate(&self) -> ::rodio::SampleRate {
        self.decoder.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(self.end?.saturating_sub(self.start))
    }

    fn try_seek(&mut self, requested: Duration) -> Result<(), SeekError> {
        let failed = |error: Error| SeekError::Other(Arc::new(error));

        // Static seeks stop at the known end; live seeks are clamped by the stream.
        let target = self.start.saturating_add(requested);
        let target = self.end.map_or(target, |end| target.min(end));
        let (stream, segment_start) = self
            .manifest
            .segment_stream_at_with_cache(&self.id, target, self.cache.clone())
            .map_err(failed)?;
        let stream = stream.with_behind_window(self.behind);
        let target = stream
            .clamp_time(target)
            .map_err(|error| failed(error.into()))?;
        let (stream, mut decoder) = Self::decoder(stream, &self.mime_type).map_err(failed)?;

        let frames_to_skip = target
            .saturating_sub(segment_start)
            .as_nanos()
            .saturating_mul(u128::from(decoder.sample_rate().get()))
            / 1_000_000_000;
        let samples_to_skip = frames_to_skip.saturating_mul(u128::from(decoder.channels().get()));
        let samples_to_skip = usize::try_from(samples_to_skip).map_err(|_| {
            SeekError::Other(Arc::new(Error::Initialization(
                "seek offset exceeds this platform's addressable sample count".to_owned(),
            )))
        })?;

        for _ in 0..samples_to_skip {
            if decoder.next().is_none() {
                return Err(SeekError::Other(Arc::new(Error::Initialization(format!(
                    "decoder ended before requested seek position {}s",
                    requested.as_secs_f64()
                )))));
            }
        }

        // Reattach reporting before replacing the active stream and decoder.
        stream.subscribe_buffer(self.buffer.feed());
        self.stream = stream;
        self.decoder = decoder;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;
    use crate::segment::SegmentLocation;
    use crate::segment::clock::ManualClock;
    use std::sync::Mutex;

    /// Serves numbered live AAC requests from a repeating three-segment fixture
    /// and records every requested segment number.
    #[derive(Default)]
    struct LoopedAac(Mutex<Vec<u64>>);

    impl Transport for LoopedAac {
        fn get(&self, segment: &SegmentLocation, _size_hint: Option<u64>) -> Result<crate::Bytes, Error> {
            let name = segment.url.trim_start_matches("https://cdn.test/");
            let number = name
                .strip_prefix("aac_")
                .and_then(|name| name.strip_suffix(".m4s"))
                .and_then(|number| number.parse::<u64>().ok());
            let file = match number {
                Some(number) => {
                    self.0.lock().unwrap().push(number);
                    format!("aac_{}.m4s", (number - 1) % 3 + 1)
                }
                None => name.to_owned(),
            };
            let path = format!("{}/tests/files/packets/{file}", env!("CARGO_MANIFEST_DIR"));
            Ok(crate::Bytes::from(std::fs::read(path)?))
        }
    }

    #[test]
    fn a_live_source_plays_on_after_falling_behind_the_window() {
        // Use half-second AAC segments in a two-second live window.
        let xml = r#"
            <MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
                 availabilityStartTime="1970-01-01T00:00:00Z" timeShiftBufferDepth="PT2S">
              <Period start="PT0S">
                <AdaptationSet mimeType="audio/mp4">
                  <Representation id="aac" codecs="mp4a.40.2" bandwidth="24000">
                    <SegmentTemplate timescale="48000" initialization="aac_init.mp4"
                                     media="aac_$Number$.m4s" duration="24000"/>
                  </Representation>
                </AdaptationSet>
              </Period>
            </MPD>"#;
        let clock = ManualClock::at(10);
        let manifest = DashManifest::new(xml)
            .and_then(|manifest| manifest.with_base_url("https://cdn.test/live.mpd"))
            .unwrap()
            .with_clock(clock.clone());
        let transport = Arc::new(LoopedAac::default());
        let cache = Arc::new(SegmentCache::new(transport.clone()));

        // The default four-segment delay starts at the beginning of this window.
        let mut source = DashSource::new_with_cache(&manifest, "aac", cache).unwrap();
        let secs = Duration::from_secs;
        assert_eq!(source.total_duration(), None);
        assert_eq!(source.start(), secs(8));
        assert_eq!(source.available(), Some(secs(8)..secs(10)));
        assert_eq!(source.live_edge(), Some(secs(10)));

        // Move the live window ahead after playing a quarter second.
        let second = source.sample_rate().get() as usize * source.channels().get() as usize;
        assert_eq!(source.by_ref().take(second / 4).count(), second / 4);
        clock.set(20);

        // Playback resumes at the new window start without fetching expired media.
        assert_eq!(source.by_ref().take(2 * second).count(), 2 * second);
        let fetched = transport.0.lock().unwrap().clone();
        assert!(fetched.contains(&37), "{fetched:?}");
        assert!(
            !fetched.iter().any(|number| (21..37).contains(number)),
            "{fetched:?}"
        );
    }

    #[test]
    fn protected_representation_is_rejected_before_streaming() {
        let manifest = DashManifest::new(
            r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cbcs"/>
                        <Representation id="FLAC" codecs="flac"/>
                    </AdaptationSet>
                </Period>
            </MPD>
            "#,
        )
        .unwrap();

        assert!(matches!(
            DashSource::new(&manifest, "FLAC"),
            Err(Error::RodioProtectedContent(scheme)) if scheme == "cbcs"
        ));
    }

    #[test]
    fn new_with_cache_fetches_through_its_transport() {
        struct Offline;

        impl Transport for Offline {
            fn get(
                &self,
                segment: &SegmentLocation,
                _size_hint: Option<u64>,
            ) -> Result<crate::Bytes, Error> {
                Err(Error::Initialization(format!("offline: {segment}")))
            }
        }

        let manifest = DashManifest::new(
            r#"
            <MPD>
                <Period>
                    <AdaptationSet mimeType="audio/mp4">
                        <Representation id="FLAC" codecs="flac">
                            <SegmentTemplate initialization="https://cdn.example/init.mp4"
                                media="https://cdn.example/$Number$.mp4">
                                <SegmentTimeline><S d="1"/></SegmentTimeline>
                            </SegmentTemplate>
                        </Representation>
                    </AdaptationSet>
                </Period>
            </MPD>
            "#,
        )
        .unwrap();

        assert!(matches!(
            DashSource::new_with_cache(
                &manifest,
                "FLAC",
                Arc::new(SegmentCache::new(Arc::new(Offline)))
            ),
            Err(Error::Initialization(message))
                if message == "offline: https://cdn.example/init.mp4"
        ));
    }
}
