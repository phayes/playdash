//! The blocking API: fetching manifests and indexes through ureq or a
//! [`Transport`], and streaming through a [`FragmentStream`].

use std::sync::Arc;
use std::time::Duration;

use ureq::ResponseExt;

use super::DashManifest;
use crate::decoder::MediaStreamBuilder;
use crate::error::Error;
use crate::fragment::{Fragments, MediaTimeline};
use crate::stream::{FragmentCache, FragmentStream, Transport};

impl DashManifest {
    /// Fetches and parses an MPEG-DASH manifest, and loads any `SegmentBase`
    /// indexes it needs.
    ///
    /// The final URL, after redirects, becomes the [`DashManifest::base_url`]
    /// that relative `BaseURL` and segment references resolve against. `data:`
    /// URLs are decoded as with [`DashManifest::new_from_data_url`].
    pub fn new_from_url(url: &str) -> Result<Self, Error> {
        let mut manifest = match url.trim_start().starts_with("data:") {
            true => Self::new_from_data_url(url.trim())?,
            false => {
                let response = ureq::get(url).call().map_err(Error::DashManifestFetch)?;
                let final_url = response.get_uri().to_string();
                let dash_xml = response
                    .into_body()
                    .read_to_string()
                    .map_err(Error::DashManifestFetch)?;
                Self::new(dash_xml)?.with_base_url(&final_url)?
            }
        };
        manifest.load_segment_indexes(&crate::stream::ureq_agent());
        Ok(manifest)
    }

    /// Fetches every `SegmentBase` index the manifest still needs through
    /// `transport`.
    ///
    /// A representation whose index fails to load reports the failure when it
    /// is streamed; the others are unaffected. Call it again after
    /// [`DashManifest::with_base_url`] when relative index URLs had nothing to
    /// resolve against.
    pub fn load_segment_indexes(&mut self, transport: &dyn Transport) {
        for request in self.index_requests() {
            let fragment = request.fragment();
            match transport.get(fragment, fragment.known_size()) {
                Ok(body) => self.apply_index(&request, &body),
                Err(error) => self.fail_index(&request, error),
            }
        }
    }

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
    /// let stream = manifest.stream(representation.id)?;
    /// stream.start_eager_head();
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream(&self, representation_id: impl AsRef<str>) -> Result<FragmentStream, Error> {
        let (fragments, timeline) = self.stream_parts(representation_id.as_ref())?;
        FragmentStream::new(fragments, Some(timeline))
    }

    /// Like [`DashManifest::stream`], but fetches through `cache`.
    ///
    /// Pass clones of one [`FragmentCache`] to share pooled connections and
    /// downloaded fragments between streams, such as a stream rebuilt after a
    /// seek.
    pub fn stream_with_cache(
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
    /// let (stream, start) = manifest.stream_at("video", Duration::from_secs(90))?;
    /// // Decoding begins at `start`; skip what comes before 90 seconds.
    /// # drop((stream, start));
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_at(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
    ) -> Result<(FragmentStream, Duration), Error> {
        self.stream_at_with_cache(representation_id, time, Arc::default())
    }

    /// Like [`DashManifest::stream_at`], but fetches through `cache`.
    pub fn stream_at_with_cache(
        &self,
        representation_id: impl AsRef<str>,
        time: Duration,
        cache: Arc<FragmentCache>,
    ) -> Result<(FragmentStream, Duration), Error> {
        let (fragments, timeline) = self.stream_parts(representation_id.as_ref())?;
        FragmentStream::starting_at(fragments, timeline, time, cache)
    }

    /// Returns a builder of a representation's [`MediaStream`](crate::MediaStream): its
    /// track's packets in the form codec decoders read. Set options on the builder, then
    /// call [`MediaStreamBuilder::open`].
    ///
    /// The representation is matched as by [`DashManifest::representation`]. Never
    /// fails; errors come from [`MediaStreamBuilder::open`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use playdash::DashManifest;
    ///
    /// # fn example() -> Result<(), playdash::Error> {
    /// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd")?;
    /// let input = manifest.media_stream("audio").open()?;
    /// // configure the decoder with input.initialization(), then decode each packet
    /// # drop(input);
    /// # Ok(())
    /// # }
    /// ```
    pub fn media_stream(&self, representation_id: impl AsRef<str>) -> MediaStreamBuilder<'_> {
        MediaStreamBuilder::new(self, representation_id.as_ref())
    }

    fn stream_parts(&self, representation_id: &str) -> Result<(Fragments, MediaTimeline), Error> {
        let (selected, segments) = self.select_segments(representation_id)?;
        let fragments = selected.fragments(self.base_url.as_ref(), &segments)?;
        Ok((fragments, segments.media_timeline()))
    }
}
