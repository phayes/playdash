# Encrypted fixtures

A 1.5 s, 22.05 kHz mono 440 Hz FLAC sine, encrypted with test key ID
`0123456789abcdef0123456789abcdef` and key `00112233445566778899aabbccddeeff`.
`tests/encryption.rs` checks each decrypts to the same samples as `plain.mp4`.

Regenerate with ffmpeg and the shaka-packager container:

```sh
common=(-y -f lavfi -i sine=frequency=440:sample_rate=22050:duration=1.5 -c:a flac
  -movflags +frag_keyframe+empty_moov+default_base_moof -frag_duration 500000
  -fflags +bitexact -map_metadata -1)
ffmpeg "${common[@]}" plain.mp4
ffmpeg "${common[@]}" -encryption_scheme cenc-aes-ctr \
  -encryption_key 00112233445566778899aabbccddeeff \
  -encryption_kid 0123456789abcdef0123456789abcdef ffmpeg_cenc.mp4
for scheme in cbcs cenc; do
  docker run --rm -v "$PWD":/media -w /media docker.io/google/shaka-packager:latest packager \
    "in=plain.mp4,stream=audio,init_segment=shaka_${scheme}_init.mp4,segment_template=shaka_${scheme}_\$Number\$.m4s" \
    --segment_duration 0.5 --enable_raw_key_encryption --protection_scheme $scheme \
    --keys label=:key_id=0123456789abcdef0123456789abcdef:key=00112233445566778899aabbccddeeff \
    --clear_lead 0 --generate_static_live_mpd --mpd_output shaka_${scheme}.mpd
done
```
