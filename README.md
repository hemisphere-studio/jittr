<h1 align="center">jittr</h1>
<div align="center">
  <strong>
    A binary heap based jitter buffer implementation for zero latency udp/rtp streams
  </strong>
</div>
<br />
<div align="center">
  <a href="https://crates.io/crates/jittr">
    <img src="https://img.shields.io/crates/v/jittr.svg?style=flat-square"
    alt="crates.io version" />
  </a>
  <a href="https://docs.rs/jittr">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="docs.rs docs" />
  </a>
</div>
<br />

This implementation is designed to fight network jitter and create reliable
media streams through udp packets. Unordered packets and varying network delay
is one of the biggest problems when trying to constantly streaming e.g. audio
through udp (and most likely rtp). This datastructure buffers packets and
reorders them whilst introducing as little delay as possible.

It needs to know your desired sample playback rate and meta informations about
each packet to calculate how much playback time is still buffered and what the
duration of each packet is. Through this information the buffer can ratelimit
your packet consuming implementation and gets time to reorder delayed packets.

The `JitterBuffer` struct implements `Sink` and `Stream` simultaneously and is
desinged to work in async implementations. It supports all runtimes since it is
directly build ontop of the futures crate.

## Examples

### Opus

Playback opus packets (48khz / 2 channels) read from an udp/rtp stream through
the jitter buffer:

```rust
use jittr::{JitterBuffer, Packet};

let mut rtp_stream = /* your rtp stream */;

/// Your Packet implementation
struct Opus { .. }
impl Packet for Opus { .. }

/// Information about desired playback speed
const CLOCK_RATE: usize = 48000;
const CHANNELS: usize = 2;

/// Create a jitter buffer for Opus packets which can hold up to 20 packets
let mut jitter = JitterBuffer::<Opus, 20>::new(CLOCK_RATE, CHANNELS);

loop {
    futures::select! {
        rtp = rtp_stream.next().fuse() => {
            let opus: Opus = rtp.unwrap();
            jittr.send(opus).await;
        },
        next = jitter.next().fuse() => {
            log::info!("playing {}", next.unwrap().sequence_number());
            // output packet to speaker ..
        }
    }
}
```
