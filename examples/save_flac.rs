//! Saves a FLAC representation as a `.flac` file, from its decoder-ready packets: the
//! FLAC stream header its initialization holds, then every frame.
//!
//! ```text
//! cargo run --example save_flac -- [--id FORMAT] [--start SECONDS] MANIFEST_URL OUTPUT
//! ```
//!
//! Without `--id`, the manifest's first FLAC representation is saved. `--start` begins at
//! the frame playing at that time; the header still describes the whole stream, so a
//! player may report the full length. To try it on the crate's own fixture:
//!
//! ```text
//! python3 -m http.server 8000 --directory tests/files/packets &
//! cargo run --example save_flac -- http://localhost:8000/flac.mpd sine.flac
//! ```

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Duration;

use playdash::{DashManifest, DecoderFormat};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    let manifest = DashManifest::new_from_url(&args.manifest)?;
    let id = match args.id {
        Some(id) => id,
        None => manifest
            .representations()
            .find(|representation| {
                representation
                    .codecs
                    .is_some_and(|codecs| codecs.eq_ignore_ascii_case("flac"))
            })
            .ok_or("MPEG-DASH manifest has no FLAC representation")?
            .id
            .to_owned(),
    };

    // FLAC frames with the stream header.
    let builder = manifest.media_stream(&id).format(DecoderFormat::Flac);
    let input = match args.start {
        Some(seconds) => {
            // FLAC frames decode on their own, so there is no pre-roll to write.
            let time = Duration::from_secs_f64(seconds);
            let (input, seeked) = builder.open_at(time)?;
            let landed = seeked.landed_time().unwrap_or_default();
            println!("starting at {:.3}s", landed.as_secs_f64());
            input
        }
        None => builder.open()?,
    };

    // The header, then the frames: a FLAC file.
    let mut file = BufWriter::new(File::create(&args.output)?);
    file.write_all(&input.initialization().data)?;
    let mut frames = 0;
    for packet in input {
        file.write_all(&packet?.data)?;
        frames += 1;
    }
    file.flush()?;
    println!(
        "wrote {frames} frames of representation {id} to {}",
        args.output
    );
    Ok(())
}

struct Args {
    manifest: String,
    output: String,
    id: Option<String>,
    start: Option<f64>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        const USAGE: &str = "usage: save_flac [--id FORMAT] [--start SECONDS] MANIFEST_URL OUTPUT";
        let (mut positional, mut id, mut start) = (Vec::new(), None, None);
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--id" => id = Some(args.next().ok_or("--id needs a FORMAT")?),
                "--start" => {
                    let seconds = args.next().ok_or("--start needs SECONDS")?;
                    start = Some(seconds.parse().map_err(|_| "--start needs SECONDS")?);
                }
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value => positional.push(value.to_owned()),
            }
        }
        let [manifest, output] = <[String; 2]>::try_from(positional).map_err(|_| USAGE)?;
        Ok(Self {
            manifest,
            output,
            id,
            start,
        })
    }
}
