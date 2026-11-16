# meow-media

The hardware media stack shared by meow apps (crate `meow-media`, library `meow_media`):
H.264 through the SoC's decoder (rkvdec, V4L2 stateless), AAC in software, sound on the HDMI
sound card, pictures on the display's primary plane with the app's UI on the overlay plane, and
a player that keeps sound and picture together. Extracted from the YouTube app
([../youtube](https://github.com/meowcast/apps-extra/blob/main/youtube/README.md)), which is its first user.

An app that uses it needs the `media` permission ([../README.md](../README.md#permissions)), a
`MEDIA=1` firmware, and in `Cargo.toml`: `meow-media = { path = "." }`.

## Modules

| Module | What it is |
| --- | --- |
| `player` | `play`: decode thread, audio thread, presenter; the `VideoSource`/`AudioSource` traits it pulls from; push queues for live sources; `Control`, `Report` |
| `es` | Elementary-stream framing: Annex B to length-prefixed NAL units, SPS/PPS from a stream or an `avcC` record, ADTS headers |
| `mp4` | Fragmented MP4: init segment (`avcC`, `esds`, edit list), segment index (`sidx`), fragments (`moof`/`trun`) |
| `h264` | SPS/PPS/slice headers, picture order count, reference marking (`Tracker`): what a stateless decoder must be told. Progressive video only |
| `v4l2`, `decoder` | The kernel's stateless H.264 interface, one request per picture; decoded pictures leave as dmabufs (`Decoder`) |
| `audio` | `Aac` (symphonia, AAC-LC, stereo out), `Pcm` (the sound card through the kernel's PCM ioctls, 16-bit stereo), `wav` |
| `video` | `Display`: NV12 pictures on the primary plane, an ARGB UI on the overlay plane, both committed at a vertical blank |
| `canvas` | `Canvas`: CPU drawing for the UI (dithered gradients, anti-aliased shapes, text, PNG) |
| `png`, `image` | Pictures for the UI: `image::Decoder` reads the format from the file's first bytes, decodes PNG here (`png`, no dependency) and hands WebP and JPEG to the system image service ([../README.md](../README.md#the-image-service)), which the system loads from the store when first needed |

Kernel struct layouts (`v4l2`, `audio`) were read from the Linux 7.3.4 headers with the cross
compiler and are asserted at build time for the dongle's target.

## Playing something

`player::play` blocks on the calling thread (it presents) until both sources end,
`control.stop` is set, or something fails. It pulls H.264 access units from a `VideoSource` and
sound from an optional `AudioSource`; both may block, which is how a source paces the player.

```rust
use meow_media::player::{self, AudioSetup, Control, Options, Sync, VideoSetup};
use meow_media::video::Display;

let display = Display::open(Some(card))?;      // card: the DRM fd from the SDK's display grant
let control = Control::new(volume, 0.0);      // the system volume 0-100 (SDK); stop, paused, position()
```

### Pull: a file or segments (what YouTube does)

Implement the two traits over your demuxer. Each `next` returns one sample, borrowed from the
source, in decoding order. A demuxer that knows what follows gives `horizon` (no later picture
is presented before that time), so pictures are released in order without added delay.
`../youtube/src/player.rs` is the model: two downloader threads feed `Segments`, which
implements both traits over `mp4::parse_fragment`.

```rust
let setup = VideoSetup::from_track(&video_track)?;                   // mp4::parse_init
let sound = AudioSetup::Aac { config, sample_rate };                 // from the audio track
let options = Options { start, sync: Sync::Audio, stall: Some(Duration::from_secs(8)) };
let report = player::play(&mut pictures, &setup, Some((&mut samples, &sound)), &options, &control, &display)?;
```

`start` is the position asked for: samples before it (the segment begins earlier) are decoded
but not shown or heard.

### Push: a live elementary stream (HLS/TS, AirPlay mirroring, Miracast)

The app's network thread pushes timed units into bounded queues; the queues are the sources.

```rust
// Wait for parameter sets: an Annex B stream repeats SPS/PPS before every IDR picture.
let mut setup = VideoSetup::from_annexb(&first_access_unit).ok_or("no SPS/PPS yet")?;
// (AirPlay sends an avcC record instead: VideoSetup::from_avcc_record(&record)?)
setup.reorder = 2;                                 // only for streams with B-pictures (broadcast)
let (video, mut pictures) = player::video_queue(4);   // access units the app may be ahead
let (audio, mut samples) = player::audio_queue(8);
let sound = AudioSetup::Aac { config: adts.config.to_vec(), sample_rate: adts.sample_rate };

std::thread::scope(|scope| {
    scope.spawn(|| {
        // for each access unit / PES packet received:
        video.push_annexb(pts, &access_unit);      // or push(pts, length_prefixed)
        audio.push_adts(pts, &adts_frames);        // or push(pts, raw_aac_frame), push_pcm(pts, &i16s)
        // dropping `video` and `audio` ends the stream; a false return means play has ended
    });
    player::play(&mut pictures, &setup, Some((&mut samples, &sound)), &Options::live(Sync::Arrival), &control, &display)
})
```

- **Framing.** Video: one access unit per push, Annex B (`push_annexb`) or NAL units behind
  4-byte lengths (`push`; other length sizes via `VideoSetup::nal_length_size`). Audio: raw AAC
  frames with an AudioSpecificConfig (`es::adts_header` gives one for ADTS), or 16-bit stereo
  PCM (`AudioSetup::Pcm`; decode ALAC or anything else in the app and push PCM).
- **Sync.** `Sync::Audio`: the sound heard is the clock (video on demand, live TV; follows
  timestamp jumps as soon as the sound does). `Sync::WallClock`: timestamps against the system
  clock from the first picture, restarted when they jump by more than a second (also what
  `Audio` becomes without sound). `Sync::Arrival`: each picture at the first vertical blank
  after it is decoded, sound as it comes: the lowest latency, for sources already paced in
  real time (mirroring).
- **Backpressure and latency.** `push` blocks while the queue is full; `try_push` drops
  instead and says so (`Pushed::Full`). One thread that fills both queues uses
  `push_until(pts, unit, &control.stop)`: after a stop nobody takes from the full queue it
  waits on, while `play` waits for the other queue to end, which only that thread can do. A dropped picture damages those that refer to it, so
  after a drop skip to the next IDR picture or ask the sender for one. The sound card's ring
  (186 ms at 44.1 kHz) is the only other buffer.
- **Joining and changes.** Pictures before the first IDR picture are skipped. When the picture
  size changes (a phone rotates), `play` returns `Err(player::FORMAT_CHANGED)`: call it again
  with the new parameter sets. `Options::live` never reports a stall, since a still screen
  sends nothing.
- **Reordering.** Without a `horizon`, pictures come out in decoding order unless
  `VideoSetup::reorder` holds some back (at most `MAX_REORDER` = 4; each adds a frame of delay).

Sound without pictures does not need the player: `audio::Aac` and `audio::Pcm` directly
(`Pcm::open(rate)`, `write(&[i16])`, `delay()`, `pause`, `drain`).

## The display and the UI

`video::Display::open(card)` takes the DRM device (None from a root shell; the SDK's display
grant in an app). The player owns the picture plane while `play` runs; the UI overlay is the
app's at all times:

```rust
let mut canvas = meow_media::canvas::Canvas::new(display.width as usize, display.height as usize);
canvas.text(&font, 40.0, 64.0, 96.0, 0xffff_ffff, "Hello");   // colours: 0xAARRGGBB
display.publish(&canvas, 0, true)?;   // shown at the next vertical blank; `top`: slide offset in rows
display.set_visible(false)?;          // the bare picture
```

While `play` runs it commits UI changes together with pictures; when nothing is playing,
`publish` and `set_visible` commit by themselves. `detach(true)` (through
`Control::hold_picture`) leaves the last picture up for the next `play`; `blank()` clears it.

**A UI that animates** (the sweet.tv app's channel list) uses `publish_painted` instead:

```rust
let panel = Placement { area: Rect::new(0, 0, 1320, 1080), from: Edge::Left, travel: 1320, duration: Duration::from_millis(200) };
display.publish_painted(panel, &damage, more, &mut |frame: &mut Frame, part: Option<Rect>| {
    if let Some(part) = part { frame.copy(&canvas, part) }   // None: last call, see below
})?;
while display.animate()? {}           // without a player: moves the slide on, a blank per call
```

- **Only what changed.** The two UI buffers take turns, so each is two frames behind. The
  display remembers `damage` for both (overlapping parts become one, all clipped to the
  placement's area) and calls the painter with every part the buffer in turn lacks; then
  once with None, for a painter that keeps track itself of what each buffer
  (`Frame::buffer`) holds and writes only the difference. Writing to these buffers is the
  cost of a frame: single runs of a few pixels go at 60 to 130 MB a second on the RK3228 at
  408 to 600 MHz, whole rows at 350. `publish` copies the whole canvas every time.
- **One frame per vertical blank.** The call returns when the frame before is on screen
  (`Display::pacing` says how long that took and how long the painting). A running player
  waits for the UI's frame until 4 ms before the blank (`hold_for_ui`), so that picture and
  UI frame go up in one commit; a frame painted in under 8 ms or so makes every blank.
  `publish` waits 20 ms after every commit, which is 30 frames a second at best.
- **Slides for nothing.** `Placement` says which part of the screen the UI is in and which
  edge it comes from (`Edge::Bottom`, `Left`, `Right`, `None`): the display controller moves
  and crops the plane, no pixel is drawn. `set_visible(false)` slides it back out.

## Limits

- H.264 only, progressive, up to what rkvdec decodes (1080p60 High 4.2 measured: 11 ms per
  picture). AAC-LC only; HE-AAC plays at half bandwidth. Output is always 16-bit stereo.
- One `play` at a time per process (one decoder, one sound card stream, one picture plane).
- A stream error (a lost reference, a broken slice) ends `play` with an error; live apps start
  it again at the next IDR picture.

## Tests

```sh
# In the builder container, from apps (host target; name the package):
cargo test -p meow-media --target aarch64-unknown-linux-gnu --offline --config .cargo/vendor.toml
```
