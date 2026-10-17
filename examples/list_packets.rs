//! Prints a representation's codec and packets, optionally from a seek.
//!
//! ```text
//! cargo run --example list_packets -- [--id FORMAT] [--seek SECONDS] [--fragment] MANIFEST_URL
//! ```
//!
//! Without `--id` the manifest's first representation is read. `--seek` seeks to the
//! packet at that time, or with `--fragment` to the start of its fragment, and lists the
//! pre-roll packets first. For example:
//!
//! ```text
//! cargo run --example list_packets -- --id bbb_a64k --seek 30 https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd
//! ```

use std::error::Error;
use std::time::Duration;

use playdash::{DashManifest, Packet, PacketStream, TimeAccuracy};

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
            .next()
            .ok_or("MPEG-DASH manifest has no representations")?
            .id
            .to_owned(),
    };

    let mut packets = PacketStream::new(manifest.stream(&id)?)?;
    println!("representation {id}, track {}", packets.track_id());
    println!("codec {:?}", packets.codec());
    println!(
        "timescale {}, pre-roll {:?}",
        packets.timescale(),
        packets.preroll()
    );

    if let Some(seconds) = args.seek {
        let accuracy = match args.fragment {
            true => TimeAccuracy::Fragment,
            false => TimeAccuracy::Packet,
        };
        let seeked = packets.seek_time(Duration::from_secs_f64(seconds), accuracy)?;
        println!(
            "seeked to {:?}: landed at {}, discarding output before {}",
            seeked.requested, seeked.landed, seeked.discard_before
        );
        for packet in &seeked.preroll {
            print_packet("pre-roll", packet);
        }
    }
    for packet in packets {
        print_packet("packet", &packet?);
    }
    Ok(())
}

fn print_packet(kind: &str, packet: &Packet) {
    println!(
        "{kind} pts {} dts {} duration {} size {}{}{}",
        packet.pts,
        packet.dts,
        packet.duration,
        packet.data.len(),
        if packet.keyframe { " keyframe" } else { "" },
        match (packet.trim_start, packet.trim_end) {
            (0, 0) => String::new(),
            (start, end) => format!(" trim {start}+{end}"),
        },
    );
}

struct Args {
    manifest: String,
    id: Option<String>,
    seek: Option<f64>,
    fragment: bool,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let (mut manifest, mut id, mut seek, mut fragment) = (None, None, None, false);
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--id" => id = Some(args.next().ok_or("--id needs a FORMAT")?),
                "--seek" => {
                    let seconds = args.next().ok_or("--seek needs SECONDS")?;
                    seek = Some(seconds.parse().map_err(|_| "--seek needs SECONDS")?);
                }
                "--fragment" => fragment = true,
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value if manifest.is_none() => manifest = Some(value.to_owned()),
                value => return Err(format!("unexpected argument: {value}").into()),
            }
        }
        Ok(Self {
            manifest: manifest.ok_or(
                "usage: list_packets [--id FORMAT] [--seek SECONDS] [--fragment] MANIFEST_URL",
            )?,
            id,
            seek,
            fragment,
        })
    }
}
