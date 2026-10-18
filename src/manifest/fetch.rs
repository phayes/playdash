//! Loading manifests and opening decoder-ready media with the blocking API.

use ureq::ResponseExt;

use super::DashManifest;
use crate::decoder::MediaStreamBuilder;
use crate::error::Error;
use crate::fragment::Transport;
use crate::fragment::transport::ureq_agent;

impl DashManifest {
    /// Downloads a manifest and prepares it for representation selection and streaming.
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

    /// Loads unresolved `SegmentBase` indexes through `transport`.
    ///
    /// Most callers do not need this because [`Self::new_from_url`] loads indexes
    /// automatically. Call it after [`DashManifest::new`] when the manifest uses
    /// `SegmentBase`, or after changing its base URL.
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

    /// Prepares one representation for decoder-ready streaming.
    ///
    /// This is the main entry point after loading a manifest. Set any options on the
    /// returned builder, then call [`MediaStreamBuilder::open`]. The resulting
    /// [`MediaStream`](crate::MediaStream) supplies decoder setup data and compressed
    /// packets as they download.
    ///
    /// `representation_id` is matched as by [`DashManifest::representation`]. Creating
    /// the builder cannot fail; downloading and media errors are reported by `open`.
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
