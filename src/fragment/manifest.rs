//! Opening a manifest's representations as blocking [`FragmentStream`]s.

use super::{FragmentCache, FragmentStream};
use crate::error::Error;
use crate::manifest::DashManifest;
use std::sync::Arc;
use std::time::Duration;

impl DashManifest {
    /// Opens a representation as a [`FragmentStream`]: an iterator of its
    /// whole fragments, and a progressive [`std::io::Read`] and
    /// [`std::io::Seek`] view of them. The initialization fragment is fetched
    /// before this returns.
    ///
    /// Use [`DashManifest::representations`] to inspect the available choices,
    /// then pass the selected representation's ID as `representation_id`. It
    /// uses the same full-ID or format-token matching as
    /// [`DashManifest::representation`].
    ///
    /// Call [`FragmentStream::start_eager_head`] to discover fragment sizes in
    /// the background, which can make byte seeking faster on servers that
    /// support `HEAD`. Timestamp seeking is available through
    /// [`FragmentStream::seek_time`].
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
    /// let stream = manifest.fragment_stream(representation.id)?;
    /// stream.start_eager_head();
    /// # Ok(())
    /// # }
    /// ```
    pub fn fragment_stream(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<FragmentStream, Error> {
        let (fragments, timeline) = self.stream_parts(representation_id.as_ref())?;
        FragmentStream::new(fragments, Some(timeline))
    }

    /// Like [`DashManifest::fragment_stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`FragmentCache`] to share pooled connections and
    /// downloaded fragments between streams, such as a stream rebuilt after a
    /// seek.
    pub fn fragment_stream_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        cache: Arc<FragmentCache>,
    ) -> Result<FragmentStream, Error> {
        let (fragments, timeline) = self.stream_parts(representation_id.as_ref())?;
        FragmentStream::new_with_cache(fragments, Some(timeline), cache)
    }

    /// Opens a representation as a [`FragmentStream`] that reads its
    /// initialization fragment, then its media from the fragment covering
    /// `time`, as a decoder starting there needs. Returns the stream and the
    /// start of that fragment, at or before `time`.
    ///
    /// The stream reports and seeks by the representation's media time. See
    /// [`FragmentStream::starting_at`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use playdash::DashManifest;
    ///
    /// # fn example() -> Result<(), playdash::Error> {
    /// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
    /// let (stream, start) = manifest.fragment_stream_at("video", Duration::from_secs(90))?;
    /// // Decoding begins at `start`; skip what comes before 90 seconds.
    /// # drop((stream, start));
    /// # Ok(())
    /// # }
    /// ```
    pub fn fragment_stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(FragmentStream, Duration), Error> {
        self.fragment_stream_at_with_cache(representation_id, time, Arc::default())
    }

    /// Like [`DashManifest::fragment_stream_at`], but fetches through `cache`.
    pub fn fragment_stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(FragmentStream, Duration), Error> {
        let (fragments, timeline) = self.stream_parts(representation_id.as_ref())?;
        FragmentStream::starting_at(fragments, timeline, time, cache)
    }
}
