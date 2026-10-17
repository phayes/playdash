//! Opening a manifest's representations as async [`FragmentStream`]s.

use super::{FragmentCache, FragmentStream};
use crate::error::Error;
use crate::nonblocking::DashManifest;
use std::sync::Arc;
use std::time::Duration;

impl DashManifest {
    /// Opens a representation as a [`FragmentStream`]: a stream of its whole
    /// fragments, and an async reader of them. The initialization fragment is
    /// fetched before this returns.
    ///
    /// The representation is matched as by [`Self::representation`]. Call
    /// [`FragmentStream::start_eager_head`] to discover fragment sizes in the
    /// background, which can make byte seeking faster on servers that support
    /// `HEAD`.
    pub async fn fragment_stream(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<FragmentStream, Error> {
        self.fragment_stream_with_cache(representation_id, Arc::new(self.new_cache()))
            .await
    }

    /// Like [`Self::fragment_stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`FragmentCache`] to share pooled connections and
    /// downloaded fragments between streams, such as a stream rebuilt after a
    /// seek.
    pub async fn fragment_stream_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<FragmentCache>,
    ) -> Result<FragmentStream, Error> {
        let (fragments, timeline) = self.manifest.stream_parts(representation_id.as_ref())?;
        FragmentStream::new_with_cache(fragments, Some(timeline), cache).await
    }

    /// Opens a representation as a [`FragmentStream`] that reads its
    /// initialization fragment, then its media from the fragment covering
    /// `time`, as a decoder starting there needs. Returns the stream and the
    /// start of that fragment, at or before `time`.
    ///
    /// The stream reports and seeks by the representation's media time. See
    /// [`FragmentStream::starting_at`].
    pub async fn fragment_stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(FragmentStream, Duration), Error> {
        self.fragment_stream_at_with_cache(representation_id, time, Arc::new(self.new_cache()))
            .await
    }

    /// Like [`Self::fragment_stream_at`], but fetches through `cache`.
    pub async fn fragment_stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(FragmentStream, Duration), Error> {
        let (fragments, timeline) = self.manifest.stream_parts(representation_id.as_ref())?;
        FragmentStream::starting_at(fragments, timeline, time, cache).await
    }

    /// A new, empty cache that fetches through this manifest's client.
    pub(crate) fn new_cache(&self) -> FragmentCache {
        FragmentCache::new(Arc::new(self.client().clone()))
    }
}
