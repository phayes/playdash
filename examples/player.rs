//! Command-line player using the DASH-aware [`DashSource`] rodio adapter.
//!
//! ```text
//! cargo run --example player -- [--id FORMAT] [MANIFEST]
//! ```
//!
//! `MANIFEST` is MPEG-DASH XML, a `data:` URL, a file path, or omitted to read stdin.

use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use rodio::{DeviceSinkBuilder, Player};
use tidal_dash::{DashManifest, DashRepresentation, DashSource};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let args = Args::parse()?;
    if args.help {
        print_usage();
        return Ok(());
    }

    let xml = load_input(args.manifest.as_deref())?;
    let manifest = parse_manifest(&xml)?;
    let representation = select_representation(&manifest, args.id.as_deref())?;
    let bitrate = representation
        .bitrate
        .ok_or_else(|| format!("representation {} has no bandwidth", representation.id))?;

    println!(
        "playing {} ({}, {} bps)",
        representation.id, representation.codecs, bitrate
    );
    print_keys();

    let source = DashSource::from_representation(representation)?;

    let device = DeviceSinkBuilder::open_default_sink()?;
    let player = Player::connect_new(device.mixer());
    player.append(source);

    let _raw = RawMode::enable()?;
    play(&player)
}

struct Args {
    help: bool,
    id: Option<String>,
    manifest: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut help = false;
        let mut id = None;
        let mut manifest = None;
        let mut args = std::env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => help = true,
                "--id" => {
                    id = Some(args.next().ok_or("--id requires a representation id")?);
                }
                flag if let Some(value) = flag.strip_prefix("--id=") => {
                    id = Some(value.to_owned());
                }
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value if manifest.is_none() => manifest = Some(value.to_owned()),
                extra => return Err(format!("unexpected argument: {extra}").into()),
            }
        }

        Ok(Self { help, id, manifest })
    }
}

fn load_input(manifest: Option<&str>) -> Result<String, Box<dyn Error>> {
    match manifest {
        None => {
            let mut xml = String::new();
            io::stdin().read_to_string(&mut xml)?;
            if xml.trim().is_empty() {
                return Err("no manifest on stdin".into());
            }
            Ok(xml)
        }
        Some(value) if value.starts_with("data:") || looks_like_xml(value) => Ok(value.to_owned()),
        Some(path) => Ok(fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", Path::new(path).display()))?),
    }
}

fn looks_like_xml(value: &str) -> bool {
    value.trim_start().starts_with('<')
}

fn parse_manifest(input: &str) -> Result<DashManifest, Box<dyn Error>> {
    if input.trim_start().starts_with("data:") {
        Ok(DashManifest::new_from_data_url(input.trim())?)
    } else {
        Ok(DashManifest::new(input.to_owned())?)
    }
}

fn select_representation<'a>(
    manifest: &'a DashManifest,
    id: Option<&str>,
) -> Result<&'a DashRepresentation, Box<dyn Error>> {
    match id {
        Some(id) => manifest
            .representation(id)
            .ok_or_else(|| format!("MPEG-DASH representation not found: {id}").into()),
        None => manifest
            .representations
            .first()
            .ok_or_else(|| "MPEG-DASH manifest contains no representations".into()),
    }
}

fn play(player: &Player) -> Result<(), Box<dyn Error>> {
    loop {
        if player.empty() {
            return Ok(());
        }

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }

        let Event::Key(KeyEvent {
            code,
            kind: KeyEventKind::Press,
            ..
        }) = event::read()?
        else {
            continue;
        };

        match code {
            KeyCode::Left => seek(player, -1),
            KeyCode::Right => seek(player, 1),
            KeyCode::Down => seek(player, -60),
            KeyCode::Up => seek(player, 60),
            KeyCode::Char(' ') => {
                if player.is_paused() {
                    player.play();
                } else {
                    player.pause();
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
            _ => {}
        }
    }
}

fn seek(player: &Player, delta_secs: i64) {
    let pos = player.get_pos();
    let target = if delta_secs >= 0 {
        pos.saturating_add(Duration::from_secs(delta_secs as u64))
    } else {
        pos.saturating_sub(Duration::from_secs(delta_secs.unsigned_abs()))
    };
    if let Err(error) = player.try_seek(target) {
        eprintln!("\rseek to {}s failed: {error}\r", target.as_secs());
    }
}

fn print_usage() {
    println!(
        "\
Usage: player [--id FORMAT] [MANIFEST]

Play a TIDAL MPEG-DASH manifest through rodio's Symphonia decoder.

MANIFEST is MPEG-DASH XML, a data: URL, a file path, or omitted to read stdin."
    );
    print_keys();
}

fn print_keys() {
    println!(
        "\
Keys:
  Left / Right   jump 1 second
  Up / Down      jump 1 minute
  Space          pause / resume
  q              quit"
    );
}

struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
