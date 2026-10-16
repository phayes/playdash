//! Windowed video player: playdash downloads and seeks, the `ffmpeg` command
//! decodes video, [`DashSource`] plays audio through rodio, and minifb draws.
//!
//! ```text
//! cargo run --release --example video_player --features rodio -- [--id FORMAT] [--audio-id FORMAT] MANIFEST_URL
//! ```
//!
//! Build with `--release`: a debug build cannot convert frames fast enough
//! and the video falls behind the audio. Requires `ffmpeg` on `PATH`. Video
//! plays at 30 frames per second, letterboxed into a 1280x720 window, and
//! follows the audio's position.
//! Without `--id` or `--audio-id` the first video or audio representation in
//! the manifest plays.
//!
//! ```text
//! # Big Buck Bunny, H.264 and HE-AAC, SegmentTemplate with $Number$
//! cargo run --example video_player --release --features rodio -- --id bbb_30fps_1280x720_4000k https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd
//!
//! # Shaka Player's "Angel One", H.264 and AAC-LC, SegmentBase (sidx index)
//! cargo run --example video_player --release --features rodio -- https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd
//! ```
//!
//! Symphonia decodes only the AAC-LC core of HE-AAC streams, so they play at
//! half the sample rate without the SBR high band.

use std::error::Error;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use playdash::{
    BufferHandle, BufferStatus, ContentKind, DashManifest, DashSource, FragmentCache,
    FragmentStream, Representation, Retention,
};
use rodio::{DeviceSinkBuilder, Player};

const WIDTH: usize = 1280;
const HEIGHT: usize = 720;
const FPS: u32 = 30;
const REDRAW_FPS: usize = 60;
const BAR_HEIGHT: usize = 6;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    // Pick the representations.
    let args = Args::parse()?;
    let manifest = DashManifest::new_from_url(&args.manifest)?;
    let video = select(&manifest, args.id.as_deref(), ContentKind::Video)?;
    let audio = select(&manifest, args.audio_id.as_deref(), ContentKind::Audio)?;
    let id = video.id;
    let total = manifest.media_timeline(id)?.total_duration();
    for representation in [&video, &audio] {
        let size = match (representation.width, representation.height) {
            (Some(width), Some(height)) => format!(", {width}x{height}"),
            _ => String::new(),
        };
        println!(
            "playing {} ({}{size})",
            representation.id,
            representation.codecs.unwrap_or("unknown codec")
        );
    }
    print_keys();

    // Audio sets the clock the video follows.
    let device = DeviceSinkBuilder::open_default_sink()?;
    let player = Player::connect_new(device.mixer());
    let audio_source = DashSource::new(&manifest, audio.id)?;
    let audio_buffer = audio_source.buffer();
    player.append(audio_source);

    // Every video decoder, including those started by a seek, shares one cache.
    let cache = Arc::new(FragmentCache::default());
    let mut decoder = Decoder::start(&manifest, id, &cache, Duration::ZERO)?;

    let mut window = Window::new("playdash", WIDTH, HEIGHT, WindowOptions::default())?;
    window.set_target_fps(REDRAW_FPS);
    let mut screen = vec![0u32; WIDTH * HEIGHT];
    let mut mouse_was_down = false;

    while window.is_open()
        && !window.is_key_down(Key::Escape)
        && !window.is_key_down(Key::Q)
        && !player.empty()
    {
        // Keys and clicks on the progress bar.
        let mut seek_to = None;
        let position = player.get_pos();
        if window.is_key_pressed(Key::Space, KeyRepeat::No) {
            if player.is_paused() {
                player.play();
            } else {
                player.pause();
            }
        }
        for (key, delta) in [
            (Key::Left, -10),
            (Key::Right, 10),
            (Key::Down, -60),
            (Key::Up, 60),
        ] {
            if window.is_key_pressed(key, KeyRepeat::Yes) {
                seek_to = Some(offset(position, delta));
            }
        }
        let mouse_down = window.get_mouse_down(MouseButton::Left);
        if mouse_down
            && !mouse_was_down
            && let (Some(total), Some((x, y))) = (total, window.get_mouse_pos(MouseMode::Discard))
            && y as usize >= HEIGHT - BAR_HEIGHT * 3
        {
            seek_to = Some(total.mul_f32(x / WIDTH as f32));
        }
        mouse_was_down = mouse_down;

        // A seek moves the audio exactly, and replaces the video decoder with
        // one starting at the fragment containing the target.
        if let Some(target) = seek_to {
            let target = total.map_or(target, |total| target.min(total));
            let result = player
                .try_seek(target)
                .map_err(Box::<dyn Error>::from)
                .and_then(|()| Decoder::start(&manifest, id, &cache, target));
            match result {
                Ok(next) => decoder = next,
                Err(error) => eprintln!("seek to {}s failed: {error}", target.as_secs()),
            }
        }

        // Show the latest frame due by the audio's position, dropping any
        // that are late.
        let clock = player.get_pos();
        while decoder.next_frame_time() <= clock {
            match decoder.frames.try_recv() {
                Ok(frame) => {
                    screen = frame;
                    decoder.taken += 1;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        // Draw, with how far both streams are buffered ahead of the clock.
        if let Some(total) = total {
            let mut statuses = vec![audio_buffer.status()];
            statuses.extend(decoder.buffer.as_ref().map(BufferHandle::status));
            let buffered = BufferStatus::buffered_until_all(&statuses, clock);
            draw_progress(&mut screen, clock, buffered, total);
        }
        window.set_title(&title(clock, total, player.is_paused()));
        window.update_with_buffer(&screen, WIDTH, HEIGHT)?;
    }
    Ok(())
}

fn select<'a>(
    manifest: &'a DashManifest,
    id: Option<&str>,
    kind: ContentKind,
) -> Result<Representation<'a>, Box<dyn Error>> {
    match id {
        Some(id) => manifest
            .representation(id)
            .ok_or_else(|| format!("MPEG-DASH representation not found: {id}").into()),
        None => manifest
            .representations()
            .find(|representation| representation.kind == kind)
            .ok_or_else(|| format!("MPEG-DASH manifest has no {kind:?} representation").into()),
    }
}

/// An `ffmpeg` process decoding one representation from a start time, and its
/// frames as 0RGB pixels.
struct Decoder {
    child: Child,
    frames: Receiver<Vec<u32>>,
    /// Media time of the first frame: the start of the fragment seeked to.
    start: Duration,
    /// Frames taken from `frames` so far.
    taken: u32,
    /// How much of the stream feeding ffmpeg is downloaded.
    buffer: Option<BufferHandle>,
}

impl Decoder {
    fn start(
        manifest: &DashManifest,
        id: &str,
        cache: &Arc<FragmentCache>,
        time: Duration,
    ) -> Result<Self, Box<dyn Error>> {
        // A stream from `time`: the init segment, then the media from the
        // fragment containing `time`, as ffmpeg needs.
        let (stream, start) = manifest.stream_at_with_cache(id, time, Arc::clone(cache))?;
        let stream = stream.with_retention(Retention {
            behind: Some(Duration::from_secs(30)),
            ahead: Some(Duration::from_secs(60)),
        });
        let buffer = stream.buffer();

        // ffmpeg reads fMP4 on stdin and writes fixed-size frames on stdout.
        let filter = format!(
            "fps={FPS},scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,pad={WIDTH}:{HEIGHT}:-1:-1"
        );
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i", "pipe:0", "-an"])
            .args([
                "-vf", &filter, "-pix_fmt", "bgr0", "-f", "rawvideo", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run ffmpeg: {error}"))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");

        // One thread feeds ffmpeg, another collects its frames. Both end when
        // the process is killed.
        thread::spawn(move || feed(stdin, stream));
        let (sender, frames) = mpsc::sync_channel(FPS as usize);
        thread::spawn(move || collect(stdout, sender));

        Ok(Self {
            child,
            frames,
            start,
            taken: 0,
            buffer,
        })
    }

    fn next_frame_time(&self) -> Duration {
        self.start + Duration::from_secs(self.taken.into()) / FPS
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn feed(mut stdin: impl Write, mut stream: FragmentStream) {
    if let Err(error) = io::copy(&mut stream, &mut stdin)
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("stream failed: {error}");
    }
}

fn collect(mut stdout: ChildStdout, sender: SyncSender<Vec<u32>>) {
    let mut bytes = vec![0u8; WIDTH * HEIGHT * 4];
    while stdout.read_exact(&mut bytes).is_ok() {
        let frame = bytes
            .chunks_exact(4)
            .map(|pixel| u32::from_le_bytes(pixel.try_into().unwrap()))
            .collect();
        if sender.send(frame).is_err() {
            return;
        }
    }
}

fn offset(position: Duration, delta_secs: i64) -> Duration {
    if delta_secs >= 0 {
        position.saturating_add(Duration::from_secs(delta_secs as u64))
    } else {
        position.saturating_sub(Duration::from_secs(delta_secs.unsigned_abs()))
    }
}

fn draw_progress(screen: &mut [u32], position: Duration, buffered: Duration, total: Duration) {
    let x =
        |time: Duration| (WIDTH as f64 * time.div_duration_f64(total)).min(WIDTH as f64) as usize;
    let (played, buffered) = (x(position), x(buffered).max(x(position)));
    for row in screen.chunks_exact_mut(WIDTH).skip(HEIGHT - BAR_HEIGHT) {
        row[..played].fill(0x00e0_3030);
        row[played..buffered].fill(0x0090_9090);
        row[buffered..].fill(0x0040_4040);
    }
}

fn title(position: Duration, total: Option<Duration>, paused: bool) -> String {
    let clock = |time: Duration| format!("{}:{:02}", time.as_secs() / 60, time.as_secs() % 60);
    let mut title = format!("playdash  {}", clock(position));
    if let Some(total) = total {
        title += &format!(" / {}", clock(total));
    }
    if paused {
        title += "  (paused)";
    }
    title
}

struct Args {
    id: Option<String>,
    audio_id: Option<String>,
    manifest: String,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut id = None;
        let mut audio_id = None;
        let mut manifest = None;
        let mut args = std::env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--id" => {
                    id = Some(args.next().ok_or("--id requires a representation id")?);
                }
                flag if let Some(value) = flag.strip_prefix("--id=") => {
                    id = Some(value.to_owned());
                }
                "--audio-id" => {
                    audio_id = Some(
                        args.next()
                            .ok_or("--audio-id requires a representation id")?,
                    );
                }
                flag if let Some(value) = flag.strip_prefix("--audio-id=") => {
                    audio_id = Some(value.to_owned());
                }
                flag if flag.starts_with('-') => {
                    return Err(format!("unknown option: {flag}").into());
                }
                value if manifest.is_none() => manifest = Some(value.to_owned()),
                extra => return Err(format!("unexpected argument: {extra}").into()),
            }
        }

        let manifest = manifest.ok_or("missing MANIFEST_URL; see --help")?;
        Ok(Self {
            id,
            audio_id,
            manifest,
        })
    }
}

fn print_usage() {
    println!(
        "\
Usage: video_player [--id FORMAT] [--audio-id FORMAT] MANIFEST_URL

Play an MPEG-DASH manifest in a window, decoding video with ffmpeg and audio
with rodio's Symphonia decoder."
    );
    print_keys();
}

fn print_keys() {
    println!(
        "\
Keys:
  Left / Right   jump 10 seconds
  Up / Down      jump 1 minute
  Click bar      seek
  Space          pause / resume
  q / Esc        quit"
    );
}
