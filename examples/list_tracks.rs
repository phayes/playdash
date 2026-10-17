//! Prints the MP4 tracks of each representation of an MPEG-DASH manifest as JSON.
//!
//! ```text
//! cargo run --example list_tracks -- [--id FORMAT] MANIFEST_URL
//! ```
//!
//! Most representations hold one track. A muxed one holds several, which
//! `PacketStream::with_track` reads one at a time. For example, GPAC's conformance
//! stream muxing H.264 and AAC in each representation:
//!
//! ```text
//! cargo run --example list_tracks -- https://download.tsi.telecom-paristech.fr/gpac/DASH_CONFORMANCE/TelecomParisTech/mp4-live-mux/mp4-live-mux-mpd-AV-NBS.mpd
//! ```

use std::error::Error;
use std::time::Duration;

use playdash::packet::Track;
use playdash::{Codec, DashManifest, Retention};
use serde_json::{Value, json};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    let manifest = DashManifest::new_from_url(&args.manifest)?;
    let ids: Vec<String> = match args.id {
        Some(id) => vec![id],
        None => manifest
            .representations()
            .map(|representation| representation.id.to_owned())
            .collect(),
    };

    let listed: Vec<Value> = ids
        .iter()
        .map(|id| match tracks(&manifest, id) {
            Ok(tracks) => json!({
                "representation": id,
                "tracks": tracks.iter().map(track_json).collect::<Vec<_>>(),
            }),
            Err(error) => json!({ "representation": id, "error": error.to_string() }),
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&listed)?);
    Ok(())
}

/// The tracks of representation `id`, from its initialization fragment.
fn tracks(manifest: &DashManifest, id: &str) -> Result<Vec<Track>, Box<dyn Error>> {
    // Opening a stream fetches the initialization fragment. Keep its download of
    // media to the first fragment, which dropping the stream then abandons.
    let stream = manifest.fragment_stream(id)?.with_retention(Retention {
        behind: Some(Duration::ZERO),
        ahead: Some(Duration::ZERO),
    });
    Ok(stream.tracks()?)
}

fn track_json(track: &Track) -> Value {
    json!({
        "id": track.id,
        "kind": track.kind,
        "timescale": track.timescale,
        "codec": codec_json(&track.codec),
    })
}

/// A summary of `codec`: its name and parameters, with the size of its decoder
/// configuration rather than the bytes.
fn codec_json(codec: &Codec) -> Value {
    let audio = |name: &str, audio: &playdash::codec::AudioConfig| {
        json!({
            "name": name,
            "sample_rate": audio.sample_rate,
            "channels": audio.channels,
            "config_bytes": audio.config.len(),
        })
    };
    let video = |name: &str, video: &playdash::codec::VideoConfig| {
        json!({
            "name": name,
            "width": video.width,
            "height": video.height,
            "config_bytes": video.config.len(),
        })
    };
    match codec {
        Codec::Aac(aac) => {
            let mut summary = audio("aac", &aac.audio);
            summary["object_type"] = json!(aac.object_type);
            summary["sbr"] = json!(aac.sbr);
            summary
        }
        Codec::Mp3(config) => audio("mp3", config),
        Codec::Ac3(config) => audio("ac3", config),
        Codec::Eac3(config) => audio("eac3", config),
        Codec::Flac(config) => audio("flac", config),
        Codec::Opus(opus) => {
            let mut summary = audio("opus", &opus.audio);
            summary["pre_skip"] = json!(opus.pre_skip);
            summary
        }
        Codec::Pcm(pcm) => {
            let mut summary = audio("pcm", &pcm.audio);
            summary["bits_per_sample"] = json!(pcm.bits_per_sample);
            summary["float"] = json!(pcm.float);
            summary["little_endian"] = json!(pcm.little_endian);
            summary
        }
        Codec::H264(config) => video("h264", config),
        Codec::Hevc(config) => video("hevc", config),
        Codec::Av1(config) => video("av1", config),
        Codec::Vp9(config) => video("vp9", config),
        Codec::Unknown(unknown) => json!({
            "name": "unknown",
            "fourcc": String::from_utf8_lossy(&unknown.fourcc),
        }),
        other => json!({ "name": format!("{other:?}") }),
    }
}

struct Args {
    manifest: String,
    id: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let (mut manifest, mut id) = (None, None);
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--id" => id = Some(args.next().ok_or("--id needs a FORMAT")?),
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value if manifest.is_none() => manifest = Some(value.to_owned()),
                value => return Err(format!("unexpected argument: {value}").into()),
            }
        }
        Ok(Self {
            manifest: manifest.ok_or("usage: list_tracks [--id FORMAT] MANIFEST_URL")?,
            id,
        })
    }
}
