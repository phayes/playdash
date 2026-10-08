//! Rodio source support for progressive MPEG-DASH playback.

#[cfg(feature = "encryption")]
use crate::ContentKeys;
use crate::{
    BufferHandle, DashManifest, Error, Fragment, FragmentCache, MediaTimeline, StreamReader,
};
use ::rodio::{Decoder, Source, source::SeekError};
use std::sync::Arc;
use std::time::Duration;

/// A seekable rodio [`Source`] for an MPEG-DASH representation.
///
/// Use [`DashSource::new`] for unprotected media. Common-encrypted media needs
/// the `encryption` feature and [`DashSource::new_with_keys`].
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
    fragments: Vec<Fragment>,
    cache: Arc<FragmentCache>,
    timeline: MediaTimeline,
    mime_type: String,
    decoder: Decoder<StreamReader>,
}

impl DashSource {
    /// Opens a representation from `manifest` for progressive rodio playback.
    ///
    /// `id` uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`]. Fetches through a new
    /// [`FragmentCache`] dedicated to this source.
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
        let cache = FragmentCache::default().with_keys(keys);
        Self::new_with_cache(manifest, id, Arc::new(cache))
    }

    /// Like [`DashSource::new`], but fetches through `cache`, for a custom
    /// [`crate::Transport`], content keys, or to share connections and
    /// downloaded fragments with other sources. A protected representation
    /// is rejected unless `cache` decrypts.
    pub fn new_with_cache(
        manifest: &DashManifest,
        id: impl AsRef<str>,
        cache: Arc<FragmentCache>,
    ) -> Result<Self, Error> {
        let id = id.as_ref();
        if let Some(scheme) = manifest.protection_scheme(id)?
            && !cache.decrypts()
        {
            return Err(Error::RodioProtectedContent(scheme));
        }
        let fragments = manifest.fragments(id)?;
        let timeline = manifest.media_timeline(id)?;
        if timeline.timescale == 0 {
            return Err(Error::StreamInitializationError(
                "rodio playback requires a non-zero DASH timescale".to_owned(),
            ));
        }
        let mime_type = manifest.mime_type(id)?.unwrap_or_default().to_owned();
        let decoder = Self::build_decoder(&fragments, &cache, &timeline, &mime_type, 0)?;

        Ok(Self {
            fragments,
            cache,
            timeline,
            mime_type,
            decoder,
        })
    }

    /// A handle reporting how much of this source's stream its cache holds.
    ///
    /// Get it before handing the source to rodio; it stays usable from any
    /// thread afterwards. Plays no part in playback.
    pub fn buffer(&self) -> BufferHandle {
        let media = self.fragments[1..]
            .iter()
            .map(|fragment| self.cache.data(fragment.clone()))
            .collect();
        BufferHandle::new(&self.cache, media, &self.timeline)
    }

    fn build_decoder(
        fragments: &[Fragment],
        cache: &Arc<FragmentCache>,
        timeline: &MediaTimeline,
        mime_type: &str,
        media_index: usize,
    ) -> Result<Decoder<StreamReader>, Error> {
        let mut selected = Vec::with_capacity(fragments.len() - media_index);
        selected.push(fragments[0].clone());
        selected.extend_from_slice(&fragments[media_index + 1..]);

        let selected_timeline = MediaTimeline {
            timescale: timeline.timescale,
            media_durations: timeline.media_durations[media_index..].to_vec(),
        };
        // The decoder is unseekable and seeks rebuild by timeline, so byte
        // sizes from eager HEADs would go unused.
        let reader =
            StreamReader::new_with_cache(selected, Some(selected_timeline), false, cache.clone())?;

        Ok(Decoder::builder()
            .with_data(reader)
            .with_mime_type(mime_type)
            .build()?)
    }

    fn total_duration_value(&self) -> Duration {
        self.timeline.total_duration().unwrap_or_default()
    }

    fn seek_target(&self, requested: Duration) -> (usize, Duration, Duration) {
        seek_target(&self.timeline, requested)
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
        Some(self.total_duration_value())
    }

    fn try_seek(&mut self, requested: Duration) -> Result<(), SeekError> {
        let (media_index, fragment_start, target) = self.seek_target(requested);
        let mut decoder = Self::build_decoder(
            &self.fragments,
            &self.cache,
            &self.timeline,
            &self.mime_type,
            media_index,
        )
        .map_err(|error| SeekError::Other(Arc::new(error)))?;

        let frames_to_skip = target
            .saturating_sub(fragment_start)
            .as_nanos()
            .saturating_mul(u128::from(decoder.sample_rate().get()))
            / 1_000_000_000;
        let samples_to_skip = frames_to_skip.saturating_mul(u128::from(decoder.channels().get()));
        let samples_to_skip = usize::try_from(samples_to_skip).map_err(|_| {
            SeekError::Other(Arc::new(Error::StreamInitializationError(
                "seek offset exceeds this platform's addressable sample count".to_owned(),
            )))
        })?;

        for _ in 0..samples_to_skip {
            if decoder.next().is_none() {
                return Err(SeekError::Other(Arc::new(
                    Error::StreamInitializationError(format!(
                        "decoder ended before requested seek position {}s",
                        target.as_secs_f64()
                    )),
                )));
            }
        }

        self.decoder = decoder;
        Ok(())
    }
}

fn seek_target(timeline: &MediaTimeline, requested: Duration) -> (usize, Duration, Duration) {
    let spans = timeline.spans();
    let target = requested.min(spans.last().map_or(Duration::ZERO, |span| span.end));
    // The end of the stream seeks into the last fragment.
    let index = spans
        .partition_point(|span| span.end <= target)
        .min(spans.len() - 1);
    (index, spans[index].start, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;

    fn timeline() -> MediaTimeline {
        MediaTimeline {
            timescale: 10,
            media_durations: vec![15, 20, 10],
        }
    }

    #[test]
    fn seek_target_selects_containing_fragment() {
        assert_eq!(
            seek_target(&timeline(), Duration::from_millis(2750)),
            (1, Duration::from_millis(1500), Duration::from_millis(2750))
        );
    }

    #[test]
    fn seek_target_saturates_at_end() {
        assert_eq!(
            seek_target(&timeline(), Duration::from_secs(10)),
            (2, Duration::from_millis(3500), Duration::from_millis(4500))
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
                fragment: &Fragment,
                _size_hint: Option<u64>,
            ) -> Result<Arc<[u8]>, Error> {
                Err(Error::StreamInitializationError(format!(
                    "offline: {fragment}"
                )))
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
                Arc::new(FragmentCache::new(Arc::new(Offline)))
            ),
            Err(Error::StreamInitializationError(message))
                if message == "offline: https://cdn.example/init.mp4"
        ));
    }
}
