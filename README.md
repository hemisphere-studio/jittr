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

## Examples

### Opus

Playback opus packets from an udp/rtp stream through the jitter buffer:

```rust
use async_std::stream::interval;
use std::time::Duration;
use jittr::{JitterBuffer, Packet};

let mut rtp_stream = /* your rtp stream */;

/// Your Packet implementation
struct Opus { .. }
impl Packet for Opus { .. }

/// Create a jitter buffer for Opus packets
/// It can hold up to 10 packets before it starts to discard old packets
let mut jitter = JitterBuffer::<Opus, 10>::new();

/// Create an interval for packet playback
/// A typical length for audio packets is between 10 and 20ms
let mut playback = interval(Duration::from_millis(20));

loop {
    futures::select! {
        _ = playback.next().fuse() => {
            let packet = jittr.pop();

            let pcm = /* Opus decodes audio if present or infers if none */

            // Write pcm to speaker
        },
        rtp = rtp_stream.next().fuse() => {
            let opus: Opus = rtp.unwrap();
            jittr.push(opus);
        },
    }
}
```


<br />

<h6 align="center">
    This code was originally published by HUM Systems. This repository continues the development of this library as they sadly stopped their open source efforts.
</h6>
