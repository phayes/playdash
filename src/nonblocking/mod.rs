//! Asynchronous MPEG-DASH playback on a Tokio runtime.
//!
//! Start with [`DashManifest`], choose a representation, and open its [`MediaStream`].
//! A media stream implements [`futures_util::Stream`] and yields compressed packets ready
//! for a decoder. Manifest and segment HTTP requests use [`reqwest`].
//!
//! The [`segment`] and [`packet`] modules provide lower-level access to downloaded
//! segments and MP4 packets. Most applications do not need them.
//!
//! # Examples
//!
//! ```no_run
//! use futures_util::StreamExt;
//! use playdash::nonblocking::DashManifest;
//!
//! # async fn example() -> Result<(), playdash::Error> {
//! let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
//! let mut input = manifest.media_stream("audio").open().await?;
//! // configure the decoder with input.initialization()
//! while let Some(packet) = input.next().await {
//!     let packet = packet?;
//!     // decode packet.data
//!     # drop(packet);
//! }
//! # Ok(())
//! # }
//! ```

mod decoder;
pub mod packet;
pub mod segment;

pub use decoder::{MediaStream, MediaStreamBuilder};

use crate::error::Error;
use crate::manifest::{ContentKind, Representation};
use crate::segment::Segments;
use reqwest::{Client, Response};
use segment::Transport;
use std::sync::OnceLock;

/// A parsed MPEG-DASH manifest whose representations stream asynchronously:
/// the async counterpart of the blocking
/// [`playdash::DashManifest`](crate::DashManifest), with the same constructors
/// and queries.
///
/// It fetches through a [`reqwest::Client`] of its own, with rustls, unless
/// given one with [`Self::with_client`]. Parsing never touches the network.
///
/// # Examples
///
/// ```no_run
/// use playdash::nonblocking::DashManifest;
///
/// # async fn example(client: reqwest::Client, xml: String) -> Result<(), playdash::Error> {
/// // A manifest that arrived some other way, fetched through the caller's client.
/// let manifest = DashManifest::new(xml)?
///     .with_client(client)
///     .with_base_url("https://media.example/stream.mpd")?;
/// let stream = manifest.media_stream("FLAC").open().await?;
/// # drop(stream);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct DashManifest {
    manifest: crate::DashManifest,
    /// Made on first use unless the caller gave one, so a caller that brings
    /// its own never pays for building a default.
    client: OnceLock<Client>,
}

impl DashManifest {
    /// Parses manifest XML without making a network request.
    ///
    /// Call [`Self::with_base_url`] when the XML contains relative media URLs. A
    /// `SegmentBase` manifest also needs [`Self::load_segment_indexes`] before streaming.
    pub fn new(dash_xml: impl AsRef<str>) -> Result<Self, Error> {
        crate::DashManifest::new(dash_xml).map(Self::from)
    }

    /// Parses a manifest embedded in a base64-encoded `data:` URL without network I/O.
    pub fn new_from_data_url(data_url: &str) -> Result<Self, Error> {
        crate::DashManifest::new_from_data_url(data_url).map(Self::from)
    }

    /// Downloads a manifest and prepares it for representation selection and streaming.
    ///
    /// The final URL, after redirects, becomes the [`Self::base_url`] that
    /// relative `BaseURL` and segment references resolve against. `data:` URLs
    /// are decoded as with [`Self::new_from_data_url`].
    pub async fn new_from_url(url: &str) -> Result<Self, Error> {
        let client = Client::new();
        let parsed = match url.trim_start().starts_with("data:") {
            true => crate::DashManifest::new_from_data_url(url.trim())?,
            false => {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(Response::error_for_status)
                    .map_err(Error::DashManifestRequest)?;
                let final_url = response.url().to_string();
                let dash_xml = response.text().await.map_err(Error::DashManifestRequest)?;
                crate::DashManifest::new(dash_xml)?.with_base_url(&final_url)?
            }
        };
        let mut manifest = Self::from(parsed).with_client(client);
        manifest.load_segment_indexes().await;
        Ok(manifest)
    }

    /// Uses `client` for subsequent manifest-index and media-segment requests.
    ///
    /// Configure the client before passing it to control connection pooling, proxies,
    /// timeouts, redirects, TLS, and default headers.
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = OnceLock::from(client);
        self
    }

    /// Sets the absolute source URL used to resolve relative media references.
    /// Use the manifest URL itself, not its containing directory.
    pub fn with_base_url(self, url: &str) -> Result<Self, Error> {
        Ok(Self {
            manifest: self.manifest.with_base_url(url)?,
            ..self
        })
    }

    /// Loads unresolved `SegmentBase` indexes with this manifest's HTTP client.
    ///
    /// [`Self::new_from_url`] does this automatically. Call it after [`Self::new`] for a
    /// `SegmentBase` manifest, or after changing the base URL.
    ///
    /// A representation whose index fails to load reports the failure when it
    /// is streamed; the others are unaffected. Call it again after
    /// [`Self::with_base_url`] when relative index URLs had nothing to resolve
    /// against.
    pub async fn load_segment_indexes(&mut self) {
        let client = self.client().clone();
        for request in self.manifest.index_requests() {
            let location = request.location();
            match Transport::get(&client, location, location.known_size()).await {
                Ok(body) => self.manifest.apply_index(&request, &body),
                Err(error) => self.manifest.fail_index(&request, error),
            }
        }
    }

    /// Prepares one representation for decoder-ready async streaming.
    ///
    /// This is the main entry point after loading a manifest. Set any options on the
    /// returned builder, then await [`MediaStreamBuilder::open`]. The resulting
    /// [`MediaStream`] supplies decoder setup data and compressed packets as they
    /// download.
    ///
    /// `representation_id` is matched as by [`Self::representation`]. Creating the
    /// builder cannot fail; downloading and media errors are reported by `open`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use playdash::nonblocking::DashManifest;
    ///
    /// # async fn example() -> Result<(), playdash::Error> {
    /// let manifest = DashManifest::new_from_url("https://media.example/stream.mpd").await?;
    /// let input = manifest.media_stream("audio").open().await?;
    /// // configure the decoder with input.initialization(), then decode each packet
    /// # drop(input);
    /// # Ok(())
    /// # }
    /// ```
    pub fn media_stream(&self, representation_id: impl AsRef<str>) -> MediaStreamBuilder<'_> {
        MediaStreamBuilder::new(self, representation_id.as_ref())
    }

    /// Returns the source URL used to resolve relative references, if one is known.
    pub fn base_url(&self) -> Option<&str> {
        self.manifest.base_url()
    }

    /// Lists every advertised audio, video, text, or muxed choice in manifest order.
    pub fn representations(&self) -> impl Iterator<Item = Representation<'_>> {
        self.manifest.representations()
    }

    /// Finds one advertised media choice by representation ID.
    /// Matching is case-insensitive and also accepts the token before the first comma.
    pub fn representation(&self, representation_id: impl AsRef<str>) -> Option<Representation<'_>> {
        self.manifest.representation(representation_id)
    }

    /// Lists only the representations classified as `kind`, in manifest order.
    pub fn representations_of(
        &self,
        kind: ContentKind,
    ) -> impl Iterator<Item = Representation<'_>> {
        self.manifest.representations_of(kind)
    }

    /// Chooses the `kind` representation with the highest advertised bitrate.
    /// Returns `None` if none of that kind declares a bitrate.
    pub fn highest_bandwidth(&self, kind: ContentKind) -> Option<Representation<'_>> {
        self.manifest.highest_bandwidth(kind)
    }

    /// Chooses the `kind` representation with the lowest advertised bitrate.
    /// Returns `None` if none of that kind declares a bitrate.
    pub fn lowest_bandwidth(&self, kind: ContentKind) -> Option<Representation<'_>> {
        self.manifest.lowest_bandwidth(kind)
    }

    /// Resolves a representation's initialization segment and ordered media segments.
    /// This is a lower-level query; use [`Self::media_stream`] for decoder-ready playback.
    pub fn segments(&self, representation_id: impl AsRef<str>) -> Result<Segments, Error> {
        self.manifest.segments(representation_id)
    }

    /// Returns the representation's inherited MIME type, or `Ok(None)` if undeclared.
    pub fn mime_type(&self, representation_id: impl AsRef<str>) -> Result<Option<&str>, Error> {
        self.manifest.mime_type(representation_id)
    }

    /// Returns the declared Common Encryption scheme, such as `cenc` or `cbcs`.
    /// `Ok(None)` means no compatible protection declaration was found.
    pub fn protection_scheme(
        &self,
        representation_id: impl AsRef<str>,
    ) -> Result<Option<String>, Error> {
        self.manifest.protection_scheme(representation_id)
    }

    /// Borrows the client-independent parsed manifest used by this async wrapper.
    pub fn manifest(&self) -> &crate::DashManifest {
        &self.manifest
    }

    /// Removes the async wrapper and returns its client-independent parsed manifest.
    /// The configured HTTP client is discarded.
    pub fn into_inner(self) -> crate::DashManifest {
        self.manifest
    }

    /// The client fetches go through, made now if none was given.
    fn client(&self) -> &Client {
        self.client.get_or_init(Client::new)
    }
}

impl From<crate::DashManifest> for DashManifest {
    /// Wraps a parsed manifest, to fetch through a client of its own unless
    /// given one with [`DashManifest::with_client`].
    fn from(manifest: crate::DashManifest) -> Self {
        Self {
            manifest,
            client: OnceLock::new(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::segment::{SegmentLocation, parse_byte_range};
    use bytes::Bytes;
    use futures_util::StreamExt;
    use segment::SegmentStream;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::ops::RangeInclusive;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A test server's handler for one request, returning a status and a body.
    type Respond = dyn Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) + Send + Sync;

    /// Requests a test server has answered, by path.
    pub(crate) type Counts = Arc<Mutex<HashMap<String, usize>>>;

    /// Serves HTTP on a local port, one thread per connection, and counts the
    /// requests for each path. Returns the base URL and the counts.
    pub(crate) fn serve(
        respond: impl Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) + Send + Sync + 'static,
    ) -> (String, Counts) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let respond: Arc<Respond> = Arc::new(respond);
        let counts = Counts::default();
        let served = counts.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (respond, served) = (respond.clone(), served.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let path = request_line.split(' ').nth(1).unwrap().to_owned();
                    let mut range = None;
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap() > 2 {
                        if let Some(value) =
                            header.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            range = Some(parse_byte_range(value).unwrap());
                        }
                        header.clear();
                    }
                    *served.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                    let (status, body) = respond(&path, range);
                    let head = match status {
                        302 => format!(
                            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            String::from_utf8(body.clone()).unwrap()
                        ),
                        _ => format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                    };
                    let stream = reader.get_mut();
                    stream.write_all(head.as_bytes()).unwrap();
                    if status != 302 {
                        stream.write_all(&body).unwrap();
                    }
                });
            }
        });
        (format!("http://{address}"), counts)
    }

    /// Serves each path's name as its body.
    pub(crate) fn named() -> impl Fn(&str, Option<RangeInclusive<u64>>) -> (u16, Vec<u8>) {
        |path, _| (200, path.trim_start_matches('/').as_bytes().to_vec())
    }

    /// Every item of `stream`, as text.
    pub(crate) async fn texts(stream: &mut SegmentStream) -> Vec<String> {
        stream
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect()
            .await
    }

    fn index_len(index_range: &str) -> usize {
        let range = parse_byte_range(index_range).unwrap();
        (*range.end() - *range.start() + 1) as usize
    }

    #[tokio::test]
    async fn new_from_url_follows_redirects_and_loads_indexes() {
        let (file, index_range) = crate::manifest::segment_base::tests::segment_base_file();
        let xml = format!(
            r#"<MPD><Period><AdaptationSet mimeType="audio/mp4"><Representation id="FLAC">
                <BaseURL>track.mp4</BaseURL><SegmentBase indexRange="{index_range}"/>
            </Representation></AdaptationSet></Period></MPD>"#
        );
        let served = file.clone();
        let (base, _) = serve(move |path, range| match (path, range) {
            ("/start", _) => (302, b"/audio/manifest.mpd".to_vec()),
            ("/audio/manifest.mpd", _) => (200, xml.clone().into_bytes()),
            ("/audio/track.mp4", Some(range)) => (
                206,
                served[*range.start() as usize..=*range.end() as usize].to_vec(),
            ),
            _ => (404, Vec::new()),
        });

        let manifest = DashManifest::new_from_url(&format!("{base}/start"))
            .await
            .unwrap();
        assert_eq!(
            manifest.base_url(),
            Some(format!("{base}/audio/manifest.mpd").as_str())
        );
        let segments = manifest.segments("FLAC").unwrap();
        assert_eq!(segments.media_len(), 2);
        assert_eq!(
            segments.init(),
            &SegmentLocation::with_range(format!("{base}/audio/track.mp4"), 0..=837)
        );
        let durations: Vec<u64> = segments
            .timed_media()
            .unwrap()
            .iter()
            .map(|segment| segment.duration)
            .collect();
        assert_eq!(durations, [4096, 1024]);

        // The stream reads the init, then the media past the index.
        let (init, media) = (&file[..838], &file[838 + index_len(&index_range)..]);
        let items: Vec<Bytes> = manifest
            .segment_stream("FLAC")
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;
        assert_eq!(items, [init, &media[..100], &media[100..]]);
    }

    #[tokio::test]
    async fn segment_stream_opens_a_representation_through_the_given_client() {
        let (base, _) = serve(named());
        let xml = r#"<MPD><Period><AdaptationSet mimeType="audio/mp4">
            <Representation id="FLAC,24bit">
                <SegmentTemplate initialization="init" media="$Number$">
                    <SegmentTimeline><S d="1" r="2"/></SegmentTimeline>
                </SegmentTemplate>
            </Representation>
        </AdaptationSet></Period></MPD>"#;
        let manifest = DashManifest::new(xml)
            .unwrap()
            .with_client(Client::new())
            .with_base_url(&format!("{base}/manifest.mpd"))
            .unwrap();

        let mut stream = manifest.segment_stream("FLAC").await.unwrap();
        assert_eq!(texts(&mut stream).await, ["init", "1", "2", "3"]);

        let (mut stream, start) = manifest
            .segment_stream_at("FLAC", Duration::from_millis(1_500))
            .await
            .unwrap();
        assert_eq!(start, Duration::from_secs(1));
        assert_eq!(texts(&mut stream).await, ["init", "2", "3"]);
        assert!(matches!(
            manifest.segment_stream("AAC").await,
            Err(Error::DashManifestMissingRepresentation(id)) if id == "AAC"
        ));
    }
}
