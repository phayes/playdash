//! A representation's tracks, as its initialization fragment lists them.

use super::codec::{self, Codec};
use super::demux::{default_description_index, description, media_error};
use super::init;
use crate::error::Error;
use crate::fragment::Fragment;
use crate::manifest::ContentKind;
use bytes::Bytes;

/// One track of a representation's initialization fragment.
///
/// # Examples
///
/// ```no_run
/// use playdash::packet::PacketStream;
/// use playdash::{ContentKind, DashManifest};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manifest = DashManifest::new_from_url("https://media.example/muxed.mpd")?;
/// let stream = manifest.fragment_stream("muxed")?;
/// let audio = stream
///     .tracks()?
///     .into_iter()
///     .find(|track| track.kind == ContentKind::Audio)
///     .ok_or("no audio track")?;
/// let packets = PacketStream::with_track(stream, audio.id)?;
/// # let _ = packets;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// Numeric MP4 track ID accepted by
    /// [`MediaStreamBuilder::track_id`](crate::MediaStreamBuilder::track_id) and
    /// [`PacketStream::with_track`](crate::packet::PacketStream::with_track).
    pub id: u32,
    /// Audio, video, text, or unknown media inferred from the MP4 handler.
    /// A single MP4 track is never [`ContentKind::Muxed`].
    pub kind: ContentKind,
    /// Source codec and its original MP4 decoder configuration.
    /// This remains available when the samples themselves are encrypted.
    pub codec: Codec,
    /// Number of timestamp ticks per second for this track's packets.
    pub timescale: u32,
}

/// The tracks of `init`, the body of `init_fragment`.
pub(crate) fn tracks(init: &Bytes, init_fragment: &Fragment) -> Result<Vec<Track>, Error> {
    let error = |error| media_error(init_fragment, error);
    init::parse(init)
        .map_err(error)?
        .tracks
        .iter()
        .map(|track| {
            let entry =
                description(&track.entries, default_description_index(track)).map_err(error)?;
            Ok(Track {
                id: track.id,
                kind: kind(&track.handler),
                codec: codec::codec(init, track, &entry).map_err(error)?,
                timescale: track.timescale,
            })
        })
        .collect()
}

/// The kind of media an `hdlr` handler type carries.
fn kind(handler: &[u8; 4]) -> ContentKind {
    match handler {
        b"soun" => ContentKind::Audio,
        b"vide" => ContentKind::Video,
        b"text" | b"subt" | b"sbtl" => ContentKind::Text,
        _ => ContentKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::init::tests::{SOUND, TrackSpec, audio_entry, bx, init, video_entry};

    #[test]
    fn lists_each_track_with_its_kind_and_codec() {
        let audio = [audio_entry(b"abcd", 1, 1000, &[])];
        let video = [video_entry(b"avc1", 16, 16, &[&bx(b"avcC", &[&[1]])])];
        let text = [bx(b"wvtt", &[&[0; 8]])];
        let body = Bytes::from(init(&[
            TrackSpec {
                entries: &audio,
                ..SOUND
            },
            TrackSpec {
                id: 2,
                handler: b"vide",
                timescale: 90_000,
                entries: &video,
                ..SOUND
            },
            TrackSpec {
                id: 3,
                handler: b"text",
                entries: &text,
                ..SOUND
            },
        ]));

        let tracks = tracks(&body, &Fragment::new("init")).unwrap();
        let listed: Vec<_> = tracks
            .iter()
            .map(|track| (track.id, track.kind, track.timescale))
            .collect();
        assert_eq!(
            listed,
            [
                (1, ContentKind::Audio, 44_100),
                (2, ContentKind::Video, 90_000),
                (3, ContentKind::Text, 44_100),
            ]
        );
        assert!(matches!(tracks[0].codec, Codec::Unknown(_)));
        assert!(matches!(tracks[1].codec, Codec::H264(_)));

        assert!(matches!(
            super::tracks(&Bytes::from_static(b"not mp4"), &Fragment::new("init")),
            Err(Error::MalformedMedia { .. })
        ));
    }
}
