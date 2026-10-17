# Packet fixtures

Short DASH representations of each codec `PacketStream` identifies, with ffprobe's packets
for each as the expected result. `tests/packets.rs` checks that `PacketStream` yields the same
packets, codecs and seek results.

- `aac`, `opus`, `mp3` and `h264` are multi-file `SegmentTemplate` representations with a
  `SegmentTimeline`.
- The others are single files addressed by a `SegmentList` of byte ranges, whose fixed
  `@duration` only approximates the segments' real durations. (As single files, Opus's and
  MP3's priming pushes their last packet into a fourth segment that `@duration` places at the
  end of the Period, where segments are dropped.)
- `muxed.mp4` is one fragmented file holding an H.264 track and an AAC track, which the test
  splits into fragments itself. `muxed_v.json` and `muxed_a.json` list each track's packets.
- `hevc` and `hevc_open_gop` are open-GOP HEVC: x265 starts each GOP after the first with a CRA
  picture, followed in decode order by RASL pictures that reference the GOP before it.
  ffmpeg's DASH muxer mistimes the CRA that starts `hevc`'s second fragment, giving it the
  presentation time of one of its RASL pictures, so those RASL pictures appear to present at
  or after it. `hevc_open_gop` is written as fragmented MP4 instead, which times it correctly,
  with a hand-written manifest of its byte ranges.
- `<name>.json` lists the packets ffprobe reads from the whole representation, with a CRC32 of
  each packet's data.

Regenerate from this directory with ffmpeg 8 (HE-AAC needs macOS's AudioToolbox encoder):

```sh
audio=(-f lavfi -i sine=frequency=440:sample_rate=48000:duration=1.5)
video=(-f lavfi -i testsrc=size=96x64:rate=25:duration=2)
common=(-fflags +bitexact -flags:a +bitexact -flags:v +bitexact -map_metadata -1
  -f dash -dash_segment_type mp4)
single() {
  name=$1; shift
  ffmpeg -y "$@" "${common[@]}" -single_file 1 -use_template 0 -use_timeline 1 \
    -single_file_name "$name.mp4" "$name.mpd"
}
template() {
  name=$1; shift
  ffmpeg -y "$@" "${common[@]}" -use_template 1 -use_timeline 1 \
    -init_seg_name "${name}_init.mp4" -media_seg_name "${name}_\$Number\$.m4s" "$name.mpd"
}

template aac "${audio[@]}" -c:a aac -b:a 24k -seg_duration 0.5
single he_aac "${audio[@]}" -c:a aac_at -profile:a 4 -b:a 24k -seg_duration 0.5
template opus "${audio[@]}" -c:a libopus -b:a 24k -seg_duration 0.5
single flac -f lavfi -i sine=frequency=440:sample_rate=22050:duration=1.5 -c:a flac -seg_duration 0.5
single ac3 "${audio[@]}" -c:a ac3 -b:a 64k -seg_duration 0.5
single eac3 "${audio[@]}" -c:a eac3 -b:a 64k -seg_duration 0.5
template mp3 "${audio[@]}" -c:a libmp3lame -b:a 32k -seg_duration 0.5
single pcm -f lavfi -i sine=frequency=440:sample_rate=8000:duration=1.5 -c:a pcm_s16le -seg_duration 0.5
template h264 "${video[@]}" -c:v libx264 -bf 2 -g 25 -keyint_min 25 -sc_threshold 0 -b:v 50k \
  -seg_duration 1
single hevc "${video[@]}" -c:v libx265 -tag:v hvc1 \
  -x265-params keyint=25:min-keyint=25:bframes=2 -b:v 50k -seg_duration 1
single av1 "${video[@]}" -c:v libsvtav1 -g 25 -b:v 50k -seg_duration 1
single vp9 "${video[@]}" -c:v libvpx-vp9 -g 25 -b:v 50k -seg_duration 1

probe() {
  ffprobe -v error -show_data_hash CRC32 -of json=c=1 \
    -show_entries packet=pts,dts,duration,size,flags,data_hash:packet_side_data=skip_samples "$@"
}
for name in aac opus mp3 h264; do
  cat "${name}_init.mp4" "${name}"_?.m4s > "/tmp/$name.mp4"
  probe "/tmp/$name.mp4" > "$name.json"
done
for name in he_aac flac ac3 eac3 pcm hevc av1 vp9; do
  probe "$name.mp4" > "$name.json"
done

# Then update the byte ranges in hevc_open_gop.mpd: ftyp+moov, then each moof+mdat.
ffmpeg -y "${video[@]}" -c:v libx265 -tag:v hvc1 \
  -x265-params keyint=25:min-keyint=25:bframes=2:open-gop=1 -b:v 50k \
  -fflags +bitexact -flags:v +bitexact -map_metadata -1 \
  -movflags frag_keyframe+empty_moov+default_base_moof hevc_open_gop.mp4
probe hevc_open_gop.mp4 > hevc_open_gop.json

ffmpeg -y "${video[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
  -c:v libx264 -bf 2 -g 25 -keyint_min 25 -sc_threshold 0 -b:v 50k -c:a aac -b:a 24k \
  -fflags +bitexact -flags:a +bitexact -flags:v +bitexact -map_metadata -1 \
  -movflags +frag_keyframe+empty_moov+default_base_moof -f mp4 muxed.mp4
for stream in v a; do
  probe -select_streams "$stream:0" muxed.mp4 > "muxed_$stream.json"
done
```
