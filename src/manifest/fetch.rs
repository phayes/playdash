//! The blocking API: fetching manifests and indexes through ureq or a
//! [`Transport`], and opening a [`MediaStream`](crate::MediaStream).

use ureq::ResponseExt;

use super::DashManifest;
use crate::decoder::MediaStreamBuilder;
use crate::error::Error;
use crate::fragment::Transport;
use crate::fragment::transport::ureq_agent;

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
        manifest.load_segment_indexes(&ureq_agent());
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
}
