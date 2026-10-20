//! Opening a manifest's representations as blocking [`SegmentStream`]s.

use super::{SegmentCache, SegmentStream};
use crate::error::Error;
use crate::manifest::DashManifest;
use crate::manifest::live::LiveStart;
use crate::{LiveDelay, Retention};
use std::sync::Arc;
use std::time::Duration;

impl DashManifest {
    /// Opens a representation as a [`SegmentStream`]: an iterator of its
    /// whole segments, and a progressive [`std::io::Read`] and
    /// [`std::io::Seek`] view of them. The initialization segment is fetched
    /// before this returns.
    ///
    /// Use [`DashManifest::representations`] to inspect the available choices,
    /// then pass the selected representation's ID as `representation_id`. It
    /// uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`].
    ///
    /// Call [`SegmentStream::start_eager_head`] to discover segment sizes in
    /// the background, which can make byte seeking faster on servers that
    /// support `HEAD`. Timestamp seeking is available through
    /// [`SegmentStream::seek_time`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use playdash::DashManifest;
    ///
    /// # fn example() -> Result<(), playdash::Error> {
    /// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
    ///
    /// for representation in manifest.representations() {
    ///     println!(
    ///         "{}: {:?} bps",
    ///         representation.id,
    ///         representation.bandwidth,
    ///     );
    /// }
    ///
    /// let representation = manifest
    ///     .representations()
    ///     .find(|representation| representation.bandwidth.is_some())
    ///     .expect("manifest has no streamable representation");
    /// let stream = manifest.segment_stream(representation.id)?;
    /// stream.start_eager_head();
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// For live manifests, playback starts at the configured [`LiveDelay`]
    /// and the stream refreshes the manifest automatically.
    pub fn segment_stream(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<SegmentStream, Error> {
        self.segment_stream_with_cache(representation_id, Arc::default())
    }

    /// Like [`DashManifest::segment_stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`SegmentCache`] to share pooled connections and
    /// downloaded segments between streams, such as a stream rebuilt after a
    /// seek.
    pub fn segment_stream_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<SegmentCache>,
    ) -> Result<SegmentStream, Error> {
        self.segment_stream_retained(representation_id, cache, Retention::default(), None)
    }

    /// Like [`DashManifest::segment_stream_with_cache`], keeping as much as
    /// `retention` specifies how much media to keep. For live manifests,
    /// `delay` overrides the manifest's suggested distance behind the live edge.
    pub(crate) fn segment_stream_retained(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<SegmentCache>,
        retention: Retention,
        delay: Option<LiveDelay>,
    ) -> Result<SegmentStream, Error> {
        let id = representation_id.as_ref();
        match self.live_source(id, LiveStart::Delay, delay)? {
            Some(source) => SegmentStream::new_live(source, cache, retention),
            None => SegmentStream::new_retained(self.segments(id)?, cache, retention),
        }
    }

    /// Opens a representation as a [`SegmentStream`] that reads its
    /// initialization segment, then its media from the segment covering
    /// `time`, as a decoder starting there needs. Returns the stream and the
    /// start of that segment, at or before `time`.
    ///
    /// The stream reports and seeks by the representation's media time. See
    /// [`SegmentStream::starting_at`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use playdash::DashManifest;
    ///
    /// # fn example() -> Result<(), playdash::Error> {
    /// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
    /// let (stream, start) = manifest.segment_stream_at("video", Duration::from_secs(90))?;
    /// // Decoding begins at `start`; skip what comes before 90 seconds.
    /// # drop((stream, start));
    /// # Ok(())
    /// # }
    /// ```
    pub fn segment_stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(SegmentStream, Duration), Error> {
        self.segment_stream_at_with_cache(representation_id, time, Arc::default())
    }

    /// Like [`DashManifest::segment_stream_at`], but fetches through `cache`.
    pub fn segment_stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<SegmentCache>,
    ) -> Result<(SegmentStream, Duration), Error> {
        let id = representation_id.as_ref();
        match self.live_source(id, LiveStart::At(time), None)? {
            Some(source) => {
                let start = source.start;
                let stream = SegmentStream::new_live(source, cache, Retention::default())?;
                Ok((stream, start))
            }
            None => SegmentStream::starting_at(self.segments(id)?, time, cache),
        }
    }
}
