//! Rodio source support for progressive MPEG-DASH playback.

#[cfg(feature = "encryption")]
use crate::ContentKeys;
use crate::segment::schedule;
use crate::segment::{SegmentStream, Segments};
use crate::{BufferHandle, DashManifest, Error, SegmentCache};
use ::rodio::{Decoder, Source, source::SeekError};
use std::ops::Range;
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
    segments: Segments,
    /// Presentation time of the media's start and end. Rodio positions count
    /// from the start.
    media: Range<Duration>,
    cache: Arc<SegmentCache>,
    mime_type: String,
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
        let segments = manifest.segments(id)?;
        let media = segments
            .span(0)
            .zip(segments.end())
            .map(|(first, end)| first.start..end)
            .ok_or_else(|| {
                Error::Initialization("rodio playback requires media segments".to_owned())
            })?;
        let mime_type = manifest.mime_type(id)?.unwrap_or_default().to_owned();
        let (decoder, _) = Self::build_decoder(&segments, &cache, &mime_type, media.start)?;

        Ok(Self {
            segments,
            media,
            cache,
            mime_type,
            decoder,
        })
    }

    /// A handle reporting how much of this source's stream its cache holds.
    ///
    /// Get it before handing the source to rodio; it stays usable from any
    /// thread afterwards. Plays no part in playback.
    pub fn buffer(&self) -> BufferHandle {
        // Manifest segments are timed, so every one has a span.
        let first = self.segments.numbers().start;
        let media = schedule::media_from(&self.segments, first)
            .filter_map(|(segment, span)| Some((self.cache.data(segment.clone()), span?)))
            .collect();
        BufferHandle::new(&self.cache.store, media)
    }

    /// A decoder from the media segment covering presentation time `time`,
    /// and that segment's start.
    fn build_decoder(
        segments: &Segments,
        cache: &Arc<SegmentCache>,
        mime_type: &str,
        time: Duration,
    ) -> Result<(Decoder<SegmentStream>, Duration), Error> {
        // The decoder is unseekable and seeks rebuild by timeline, so byte
        // sizes from eager HEADs would go unused.
        let (reader, start) = SegmentStream::starting_at(segments.clone(), time, cache.clone())?;

        let decoder = Decoder::builder()
            .with_data(reader)
            .with_mime_type(mime_type)
            .build()?;
        Ok((decoder, start))
    }

    fn total_duration_value(&self) -> Duration {
        self.media.end - self.media.start
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
        // Past the end, play from the end of the last segment.
        let target = self.media.start + requested.min(self.total_duration_value());
        let (mut decoder, segment_start) =
            Self::build_decoder(&self.segments, &self.cache, &self.mime_type, target)
                .map_err(|error| SeekError::Other(Arc::new(error)))?;

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

        self.decoder = decoder;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;
    use crate::segment::SegmentLocation;

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
