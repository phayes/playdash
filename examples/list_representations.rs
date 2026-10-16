//! Prints the representations of an MPEG-DASH manifest as JSON.
//!
//! ```text
//! cargo run --example list_representations -- [MANIFEST]
//! ```
//!
//! `MANIFEST` is an `http(s)` URL, MPEG-DASH XML, a `data:` URL, a file path, or
//! omitted to read stdin. For example:
//!
//! ```text
//! cargo run --example list_representations -- https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd
//! ```

use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use playdash::{DashManifest, Representation};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    if args.help {
        print_usage();
        return Ok(());
    }

    let manifest = match args.manifest.as_deref() {
        Some(url) if is_http_url(url) => DashManifest::new_from_url(url)?,
        input => parse_manifest(&load_input(input)?)?,
    };
    let representations: Vec<Representation> = manifest.representations().collect();
    println!("{}", serde_json::to_string_pretty(&representations)?);
    Ok(())
}

struct Args {
    help: bool,
    manifest: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut help = false;
        let mut manifest = None;

        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "-h" | "--help" => help = true,
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value if manifest.is_none() => manifest = Some(value.to_owned()),
                extra => return Err(format!("unexpected argument: {extra}").into()),
            }
        }

        Ok(Self { help, manifest })
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

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn looks_like_xml(value: &str) -> bool {
    value.trim_start().starts_with('<')
}

fn parse_manifest(input: &str) -> Result<DashManifest, Box<dyn Error>> {
    if input.trim_start().starts_with("data:") {
        Ok(DashManifest::new_from_data_url(input.trim())?)
    } else {
        Ok(DashManifest::new(input)?)
    }
}

fn print_usage() {
    println!(
        "\
Usage: list_representations [MANIFEST]

Print the representations of an MPEG-DASH manifest as JSON.

MANIFEST is an http(s) URL, MPEG-DASH XML, a data: URL, a file path, or omitted
to read stdin."
    );
}
