//! Rodio source support for progressive, unprotected MPEG-DASH playback.

use crate::{DashManifest, Error, MediaTimeline, MpegStreamReader};
use ::rodio::{Decoder, Source, source::SeekError};
use std::sync::Arc;
use std::time::Duration;

/// A progressive rodio source that seeks using MPEG-DASH media fragments.
///
/// The underlying Symphonia decoder is opened as a non-seekable fragmented MP4
/// stream, so it does not require a total byte length. Seeking rebuilds the
/// decoder with the initialization fragment followed by the media fragment
/// containing the requested timestamp and the remaining media fragments.
/// Common-encryption schemes such as `cenc` and `cbcs` are rejected because
/// rodio and Symphonia do not provide DRM decryption.
pub struct DashSource {
    urls: Vec<String>,
    timeline: MediaTimeline,
    mime_type: String,
    decoder: Decoder<MpegStreamReader>,
}

impl DashSource {
    /// Opens a representation from `manifest` for progressive rodio playback.
    ///
    /// `id` uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`].
    pub fn new(manifest: &DashManifest, id: impl AsRef<str>) -> Result<Self, Error> {
        let id = id.as_ref();
        if let Some(scheme) = manifest.protection_scheme(id)? {
            return Err(Error::RodioProtectedContent(scheme));
        }
        let urls = manifest.fragment_urls(id)?;
        let timeline = manifest.media_timeline(id)?;
        if timeline.timescale == 0 {
            return Err(Error::StreamInitializationError(
                "rodio playback requires a non-zero DASH timescale".to_owned(),
            ));
        }
        let mime_type = manifest.mime_type(id)?.unwrap_or_default().to_owned();
        let decoder = Self::build_decoder(&urls, &timeline, &mime_type, 0)?;

        Ok(Self {
            urls,
            timeline,
            mime_type,
            decoder,
        })
    }

    fn build_decoder(
        urls: &[String],
        timeline: &MediaTimeline,
        mime_type: &str,
        media_index: usize,
    ) -> Result<Decoder<MpegStreamReader>, Error> {
        let mut selected_urls = Vec::with_capacity(urls.len() - media_index);
        selected_urls.push(urls[0].clone());
        selected_urls.extend_from_slice(&urls[media_index + 1..]);

        let selected_timeline = MediaTimeline {
            timescale: timeline.timescale,
            media_durations: timeline.media_durations[media_index..].to_vec(),
        };
        // The decoder is unseekable and seeks rebuild by timeline, so byte
        // sizes from eager HEADs would go unused.
        let reader = MpegStreamReader::new(selected_urls, Some(selected_timeline), false)?;

        Ok(Decoder::builder()
            .with_data(reader)
            .with_mime_type(mime_type)
            .build()?)
    }

    fn total_duration_value(&self) -> Duration {
        timeline_duration(&self.timeline)
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
        let mut decoder =
            Self::build_decoder(&self.urls, &self.timeline, &self.mime_type, media_index)
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

fn duration_from_ticks(ticks: u64, timescale: u32) -> Duration {
    let timescale = u64::from(timescale);
    Duration::new(
        ticks / timescale,
        (u128::from(ticks % timescale) * 1_000_000_000 / u128::from(timescale)) as u32,
    )
}

fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
    let ticks = duration.as_nanos().saturating_mul(u128::from(timescale)) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

fn timeline_duration(timeline: &MediaTimeline) -> Duration {
    duration_from_ticks(
        timeline.media_durations.iter().copied().sum(),
        timeline.timescale,
    )
}

fn seek_target(timeline: &MediaTimeline, requested: Duration) -> (usize, Duration, Duration) {
    let target = requested.min(timeline_duration(timeline));
    let target_ticks = duration_to_ticks(target, timeline.timescale);
    let total_ticks: u64 = timeline.media_durations.iter().copied().sum();
    let mut start_ticks = 0u64;

    for (index, duration) in timeline.media_durations.iter().copied().enumerate() {
        let end_ticks = start_ticks.saturating_add(duration);
        if target_ticks < end_ticks
            || (target_ticks == total_ticks && index + 1 == timeline.media_durations.len())
        {
            let fragment_start = duration_from_ticks(start_ticks, timeline.timescale);
            return (index, fragment_start, target);
        }
        start_ticks = end_ticks;
    }

    let last = timeline.media_durations.len() - 1;
    let last_start: u64 = timeline.media_durations[..last].iter().copied().sum();
    (
        last,
        duration_from_ticks(last_start, timeline.timescale),
        target,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
