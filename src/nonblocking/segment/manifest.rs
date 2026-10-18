//! Opening a manifest's representations as async [`SegmentStream`]s.

use super::{SegmentCache, SegmentStream};
use crate::error::Error;
use crate::nonblocking::DashManifest;
use std::sync::Arc;
use std::time::Duration;

impl DashManifest {
    /// Opens a representation as a [`SegmentStream`]: a stream of its whole
    /// segments, and an async reader of them. The initialization segment is
    /// fetched before this returns.
    ///
    /// The representation is matched as by [`Self::representation`]. Call
    /// [`SegmentStream::start_eager_head`] to discover segment sizes in the
    /// background, which can make byte seeking faster on servers that support
    /// `HEAD`.
    pub async fn segment_stream(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<SegmentStream, Error> {
        self.segment_stream_with_cache(representation_id, Arc::new(self.new_cache()))
            .await
    }

    /// Like [`Self::segment_stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`SegmentCache`] to share pooled connections and
    /// downloaded segments between streams, such as a stream rebuilt after a
    /// seek.
    pub async fn segment_stream_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<SegmentCache>,
    ) -> Result<SegmentStream, Error> {
        SegmentStream::new_with_cache(self.manifest.segments(representation_id)?, cache).await
    }

    /// Opens a representation as a [`SegmentStream`] that reads its
    /// initialization segment, then its media from the segment covering
    /// `time`, as a decoder starting there needs. Returns the stream and the
    /// start of that segment, at or before `time`.
    ///
    /// The stream reports and seeks by the representation's media time. See
    /// [`SegmentStream::starting_at`].
    pub async fn segment_stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(SegmentStream, Duration), Error> {
        self.segment_stream_at_with_cache(representation_id, time, Arc::new(self.new_cache()))
            .await
    }

    /// Like [`Self::segment_stream_at`], but fetches through `cache`.
    pub async fn segment_stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<SegmentCache>,
    ) -> Result<(SegmentStream, Duration), Error> {
        let segments = self.manifest.segments(representation_id)?;
        SegmentStream::starting_at(segments, time, cache).await
    }

    /// A new, empty cache that fetches through this manifest's client.
    pub(crate) fn new_cache(&self) -> SegmentCache {
        SegmentCache::new(Arc::new(self.client().clone()))
    }
}
