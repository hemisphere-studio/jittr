use std::collections::BinaryHeap;
use std::time::SystemTime;

/// Zero latency jitter buffer for real time udp/rtp streams
pub struct JitterBuffer<P, const S: u8>
where
    P: Packet,
{
    last: Option<JitterPacket<P>>,
    heap: BinaryHeap<JitterPacket<P>>,
}

impl<P, const S: u8> JitterBuffer<P, S>
where
    P: Packet,
{
    /// Create a new jitter buffer
    pub fn new() -> Self {
        Self {
            last: None,
            heap: BinaryHeap::with_capacity(S as usize),
        }
    }

    /// Push a packet onto the jitter buffer
    ///
    /// Hint: This may drop the packet if it is already been played
    /// back or is already present in the buffer
    pub fn push(&mut self, packet: P) {
        if self.heap.len() >= S as usize {
            while self.heap.len() >= S as usize && !self.heap.is_empty() {
                // SAFETY: We just checked the length is greater or equal to 1
                let dropped = self.heap.pop();
                self.last = None;
                #[cfg(feature = "log")]
                log::warn!("dropping packet: {:?}", dropped.map(|p| p.sequence_number));
            }
        }

        if let Some(ref last) = self.last {
            if last.sequence_number >= packet.sequence_number().into() {
                #[cfg(feature = "log")]
                log::warn!(
                    "discarded packet {} since newer packet was already played back",
                    packet.sequence_number()
                );

                return;
            }
        }

        if self
            .heap
            .iter()
            .any(|p| p.sequence_number == packet.sequence_number().into())
        {
            #[cfg(feature = "log")]
            log::warn!(
                "discarded packet {} since its already buffered",
                packet.sequence_number()
            );

            return;
        }

        if !self.heap.is_empty() {
            // SAFETY: we checked that we have at least one packet in the heap
            let max_seq = self.heap.iter().max().unwrap().sequence_number;

            if SequenceNumber(max_seq.0.overflowing_add(S as u16).0)
                < packet.sequence_number().into()
            {
                #[cfg(feature = "log")]
                log::warn!(
                    "unexpectedly received packet {} which is too far ahead (over {S} packets) of current playback window, clearing jitter buffer",
                    packet.sequence_number()
                );

                self.clear();
            }
        }

        #[cfg(feature = "log")]
        log::debug!("pushed packet {} onto heap", packet.sequence_number());
        self.heap.push(packet.into());
    }

    /// Pop the next packet from the jitter buffer
    ///
    /// Hint: This will return `None` if the next packet expected
    /// (by sequence number) was lost. Most audio and video codecs used for
    /// realtime streaming support inference of lost packets.
    pub fn pop(&mut self) -> Option<P> {
        if self.heap.is_empty() {
            return None;
        }

        let last = match self.last {
            Some(ref last) => last.to_owned(),
            None => {
                // SAFETY:
                // we checked that the heap is not empty so at least one
                // element must be present or the std implementation is flawed.
                let mut packet = self.heap.pop().unwrap();
                packet.yielded_at = Some(SystemTime::now());
                self.last = Some(packet.clone());

                #[cfg(feature = "log")]
                log::debug!(
                    "packet {} yielded, {} remaining",
                    packet.sequence_number.0,
                    self.heap.len()
                );

                return packet.into();
            }
        };

        let next_sequence = match self.heap.peek() {
            Some(next) => next.sequence_number,
            None => {
                #[cfg(feature = "log")]
                log::error!("expected next packet to be present but heap is empty");

                return None;
            }
        };

        let packet = if next_sequence == (u16::from(last.sequence_number).wrapping_add(1)).into() {
            match self.heap.pop() {
                Some(packet) => packet.into(),
                None => {
                    #[cfg(feature = "log")]
                    log::error!("expected packet {} to be present", next_sequence.0);

                    return None;
                }
            }
        } else {
            None
        };

        self.last = Some(JitterPacket {
            raw: packet.clone(),
            sequence_number: packet
                .as_ref()
                .map(|p| p.sequence_number())
                .unwrap_or_else(|| u16::from(last.sequence_number).wrapping_add(1))
                .into(),
            yielded_at: Some(SystemTime::now()),
        });

        #[cfg(feature = "log")]
        log::debug!(
            "packet {:?} yielded, {} remaining",
            self.last.as_ref().map(|l| l.sequence_number),
            self.heap.len()
        );

        packet
    }

    /// Retrieve the number of packets available for playback without packet loss.
    ///
    /// Hint: Use this to reduce latency once the network is in good condition.
    /// If there are a lot of packets available for playback without packet loss
    /// it is pointless to keep them in the buffer.
    pub fn lossless_packets_buffered(&self) -> usize {
        match self.last {
            Some(ref last) => {
                let mut last = last.sequence_number;
                let mut count = 0;

                let sequence_numbers = self.heap.clone().into_sorted_vec();
                let sequence_numbers = sequence_numbers.iter().rev().map(|p| p.sequence_number);

                #[cfg(feature = "log")]
                log::debug!(
                    "compute lossless packets: {:?}",
                    sequence_numbers.clone().collect::<Vec<SequenceNumber>>()
                );

                for packet in sequence_numbers {
                    #[cfg(feature = "log")]
                    log::info!(
                        "is next of: {:?} {:?} = {}",
                        packet,
                        last,
                        packet.is_next_of(last)
                    );

                    if packet.is_next_of(last) {
                        #[cfg(feature = "log")]
                        log::debug!("{:?} is next of {:?}", packet, last);
                        last = packet;
                        count += 1;
                    } else {
                        break;
                    }
                }

                #[cfg(feature = "log")]
                log::debug!("computed lossless packets: {count}");

                count
            }
            None => 0,
        }
    }

    /// Drops all packets in the jitter buffer
    pub fn clear(&mut self) {
        self.last = None;
        self.heap.clear();
    }
}

impl<P: Packet, const S: u8> Default for JitterBuffer<P, S> {
    fn default() -> Self {
        Self::new()
    }
}

/// A packet which should be reordered and managed by the jitter buffer
pub trait Packet: Unpin + Clone {
    fn sequence_number(&self) -> u16;
}

#[derive(Debug, Clone)]
pub(crate) struct JitterPacket<P>
where
    P: Packet,
{
    pub(crate) raw: Option<P>,
    pub(crate) sequence_number: SequenceNumber,
    pub(crate) yielded_at: Option<SystemTime>,
}

impl<P> JitterPacket<P>
where
    P: Packet,
{
    fn into(self) -> Option<P> {
        self.raw
    }
}

impl<P> From<P> for JitterPacket<P>
where
    P: Packet,
{
    fn from(raw: P) -> Self {
        Self {
            sequence_number: raw.sequence_number().into(),
            yielded_at: None,
            raw: Some(raw),
        }
    }
}

impl<P> PartialEq for JitterPacket<P>
where
    P: Packet,
{
    fn eq(&self, other: &Self) -> bool {
        self.sequence_number.eq(&other.sequence_number)
    }
}

impl<P> Eq for JitterPacket<P> where P: Packet {}

impl<P> PartialOrd for JitterPacket<P>
where
    P: Packet,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<P> Ord for JitterPacket<P>
where
    P: Packet,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sequence_number.cmp(&other.sequence_number).reverse()
    }
}

/// A wrapping sequence number type according to the RFC 3550
/// that has a window in which normal u16 comparisons are inverted
///
/// See https://www.rfc-editor.org/rfc/rfc3550#appendix-A.1 as reference
/// for wrapping sequence number handling
///
/// The accepted wrapping window is set to 16 numbers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SequenceNumber(u16);

impl SequenceNumber {
    const WRAPPING_WINDOW_SIZE: u16 = 16;
    const WRAPPING_WINDOW_START: u16 = u16::MAX - (Self::WRAPPING_WINDOW_SIZE / 2);
    const WRAPPING_WINDOW_END: u16 = u16::MIN + (Self::WRAPPING_WINDOW_SIZE / 2);

    pub fn did_wrap(&self, next: Self) -> bool {
        self.0 >= Self::WRAPPING_WINDOW_START && next.0 <= Self::WRAPPING_WINDOW_END
    }

    pub fn is_next_of(&self, last: SequenceNumber) -> bool {
        if last.did_wrap(*self) {
            return last.0 == u16::MAX && self.0 == u16::MIN;
        }

        last.0.wrapping_add(1) == self.0
    }
}

impl PartialOrd for SequenceNumber {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SequenceNumber {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.did_wrap(*other) {
            return std::cmp::Ordering::Less;
        } else if other.did_wrap(*self) {
            return std::cmp::Ordering::Greater;
        }

        self.0.cmp(&other.0)
    }
}

impl From<u16> for SequenceNumber {
    fn from(num: u16) -> Self {
        Self(num)
    }
}

impl From<SequenceNumber> for u16 {
    fn from(sn: SequenceNumber) -> Self {
        sn.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Rtp {
        seq: u16,
    }

    impl Packet for Rtp {
        #[inline]
        fn sequence_number(&self) -> u16 {
            self.seq
        }
    }

    #[test]
    fn const_capacity() {
        let jitter = JitterBuffer::<Rtp, 10>::new();
        assert_eq!(jitter.heap.capacity(), 10);
    }

    #[test]
    fn send() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();
        let packet = Rtp { seq: 0 };
        jitter.push(packet.clone());
        assert_eq!(jitter.heap.peek(), Some(&packet.into()));
    }

    #[test]
    fn reorders_racing_packets() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: 0 });
        assert_eq!(jitter.pop(), Some(Rtp { seq: 0 }));

        jitter.push(Rtp { seq: 2 });
        jitter.push(Rtp { seq: 1 });

        assert_eq!(jitter.pop(), Some(Rtp { seq: 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: 2 }));
    }

    #[test]
    fn discards_already_played_packets() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: 0 });
        assert_eq!(jitter.pop(), Some(Rtp { seq: 0 }));

        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 1 });

        assert_eq!(jitter.pop(), Some(Rtp { seq: 1 }));
    }

    #[test]
    fn discards_duplicated_packets() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 0 });

        assert_eq!(jitter.pop(), Some(Rtp { seq: 0 }));
        assert_eq!(jitter.heap.len(), 0);
        assert_eq!(jitter.pop(), None);
    }

    #[test]
    fn handles_packet_loss_correctly() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: 0 });
        jitter.push(Rtp { seq: 1 });
        jitter.push(Rtp { seq: 2 });
        jitter.push(Rtp { seq: 3 });
        jitter.push(Rtp { seq: 5 });

        assert_eq!(jitter.pop(), Some(Rtp { seq: 0 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: 2 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: 3 }));
        assert_eq!(jitter.pop(), None);
        assert_eq!(jitter.pop(), Some(Rtp { seq: 5 }));
    }

    #[test]
    fn handles_wrapping_sequence_numbers() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: u16::MAX - 2 });
        jitter.push(Rtp { seq: u16::MAX - 1 });
        jitter.push(Rtp { seq: u16::MAX });
        jitter.push(Rtp { seq: u16::MIN });
        jitter.push(Rtp { seq: u16::MIN + 1 });
        jitter.push(Rtp { seq: u16::MIN + 2 });

        assert_eq!(jitter.heap.len(), 6);
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX - 2 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX - 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN + 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN + 2 }));
        assert_eq!(jitter.heap.len(), 0);
    }

    #[test]
    fn handles_reordering_of_wrapping_sequence_numbers() {
        let mut jitter = JitterBuffer::<Rtp, 10>::new();

        jitter.push(Rtp { seq: u16::MAX - 1 });
        jitter.push(Rtp { seq: u16::MIN });
        jitter.push(Rtp { seq: u16::MIN + 2 });
        jitter.push(Rtp { seq: u16::MAX - 2 });
        jitter.push(Rtp { seq: u16::MIN + 1 });
        jitter.push(Rtp { seq: u16::MAX });

        assert_eq!(jitter.heap.len(), 6);
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX - 2 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX - 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MAX }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN + 1 }));
        assert_eq!(jitter.pop(), Some(Rtp { seq: u16::MIN + 2 }));
        assert_eq!(jitter.heap.len(), 0);
    }

    mod sequence_numbers {
        use super::SequenceNumber as S;
        use std::cmp::Ordering::*;

        #[test]
        fn preserves_u16_ordering_for_non_wrapping_nums() {
            // Preserves normal ordering when not encountering wrapping
            // 1..-1 since otherwise the checks would wrap
            for i in 1..u16::MAX - 1 {
                assert_eq!(S(i - 1).cmp(&S(i)), Less);
                assert_eq!(S(i - 1).cmp(&S(i + 1)), Less);
                assert_eq!(S(i).cmp(&S(i - 1)), Greater);
                assert_eq!(S(i).cmp(&S(i)), Equal);
                assert_eq!(S(i).cmp(&S(i + 1)), Less);
                assert_eq!(S(i + 1).cmp(&S(i - 1)), Greater);
                assert_eq!(S(i + 1).cmp(&S(i)), Greater);
            }
        }

        #[test]
        fn inverts_ordering_if_wrapped() {
            for i in S::WRAPPING_WINDOW_START..u16::MAX {
                for j in u16::MIN..S::WRAPPING_WINDOW_END {
                    assert_eq!(S(i).cmp(&S(j)), Less);
                    assert_eq!(S(j).cmp(&S(i)), Greater);
                }
            }
        }

        #[test]
        fn respects_window() {
            for i in S::WRAPPING_WINDOW_START..u16::MAX {
                for j in S::WRAPPING_WINDOW_END + 1..S::WRAPPING_WINDOW_END + 8 {
                    assert_eq!(S(i).cmp(&S(j)), Greater);
                    assert_eq!(S(j).cmp(&S(i)), Less);
                }
            }
        }
    }
}
