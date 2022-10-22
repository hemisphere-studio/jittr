use futures::sink::Sink;
use futures::{FutureExt, Stream};
use futures_timer::Delay;
use std::collections::BinaryHeap;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

/// Zero latency jitter buffer for real time udp/rtp streams
pub struct JitterBuffer<P, const S: u8>
where
    P: Packet,
{
    last: Option<JitterPacket<P>>,
    delay: Option<Delay>,

    queued: Option<P>,
    heap: BinaryHeap<JitterPacket<P>>,

    sample_rate: usize,
    channels: usize,

    producer: Option<Waker>,
    consumer: Option<Waker>,
}

impl<P, const S: u8> JitterBuffer<P, S>
where
    P: Packet,
{
    const MAX_DELAY: Duration = Duration::from_millis(200);

    pub fn new(sample_rate: usize, channels: usize) -> Self {
        Self {
            last: None,
            delay: None,

            queued: None,
            heap: BinaryHeap::with_capacity(S as usize),

            sample_rate,
            channels,

            producer: None,
            consumer: None,
        }
    }

    /// Returns the calcualted packet loss ratio in this moment
    pub fn plr(&self) -> f32 {
        let buffered = self.heap.len();
        let packets_lost = self
            .heap
            .iter()
            .fold((0, 0), |(lost, last_seq), packet| {
                if last_seq == 0 {
                    return (lost, packet.sequence_number.into());
                }

                if last_seq.wrapping_add(1) != packet.sequence_number.into() {
                    // Distance between last seq + 1 and current
                    // packet goes across the u16 boundaries
                    if SequenceNumber::from(last_seq.wrapping_add(1))
                        .did_wrap(packet.sequence_number)
                    {
                        let lost_at_end = u16::MAX.saturating_sub(last_seq.wrapping_add(1));
                        let lost_at_start =
                            u16::from(packet.sequence_number).saturating_sub(u16::MIN);

                        return (
                            lost + lost_at_end + lost_at_start,
                            packet.sequence_number.into(),
                        );
                    }

                    let diff = u16::from(packet.sequence_number).saturating_sub(last_seq);

                    return (lost + diff, packet.sequence_number.into());
                }

                (lost, packet.sequence_number.into())
            })
            .0;

        #[cfg(feature = "log")]
        log::debug!(
            "packet loss ratio: {}",
            packets_lost as f32 / buffered as f32
        );

        packets_lost as f32 / buffered as f32
    }
}

impl<P, const S: u8> Sink<P> for JitterBuffer<P, S>
where
    P: Packet,
{
    type Error = ();

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.queued.is_some() {
            return Poll::Pending;
        }

        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(packet) = self.queued.take() {
            if self.heap.len() >= S as usize {
                self.queued = Some(packet);
                self.producer = Some(cx.waker().clone());
                return Poll::Pending;
            }

            if let Some(ref last) = self.last {
                if last.sequence_number >= packet.sequence_number().into() {
                    #[cfg(feature = "log")]
                    log::debug!(
                        "discarded packet {} since newer packet was already played back",
                        packet.sequence_number()
                    );

                    return Poll::Ready(Ok(()));
                }
            }

            if self
                .heap
                .iter()
                .any(|p| p.sequence_number == packet.sequence_number().into())
            {
                #[cfg(feature = "log")]
                log::debug!(
                    "discarded packet {} since its already buffered",
                    packet.sequence_number()
                );

                return Poll::Ready(Ok(()));
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

                    self.last = None;
                    self.heap.clear();
                }
            }

            self.heap.push(packet.into());

            if let Some(ref consumer) = self.consumer {
                consumer.wake_by_ref();
            }
        }

        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: P) -> Result<(), Self::Error> {
        if self.queued.is_some() {
            return Err(());
        }

        self.queued = Some(item);

        Ok(())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_flush(cx)
    }
}

impl<P, const S: u8> Stream for JitterBuffer<P, S>
where
    P: Packet,
{
    type Item = P;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.consumer.replace(cx.waker().clone());

        if self.heap.is_empty() {
            if let Some(ref producer) = self.producer {
                producer.wake_by_ref();
            }

            return Poll::Pending;
        }

        let last = match self.last {
            Some(ref last) => last.to_owned(),
            // no need to delay until the last packet is played back since
            // we are yielding the first packet right now
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

                return Poll::Ready(packet.into());
            }
        };

        // we handed a packet before, lets sleep if it is played back completly
        match self.delay.as_mut() {
            Some(ref mut delay) => match delay.poll_unpin(cx) {
                Poll::Ready(_) => {
                    self.delay = None;

                    let next_sequence = match self.heap.peek() {
                        Some(next) => next.sequence_number,
                        None => {
                            #[cfg(feature = "log")]
                            log::error!("expected next packet to be present but heap is empty");

                            return Poll::Pending;
                        }
                    };

                    let packet = if next_sequence
                        == (u16::from(last.sequence_number).wrapping_add(1)).into()
                    {
                        match self.heap.pop() {
                            Some(packet) => packet.into(),
                            None => {
                                #[cfg(feature = "log")]
                                log::error!("expected packet {} to be present", next_sequence.0);

                                return Poll::Pending;
                            }
                        }
                    } else {
                        None
                    };

                    self.last = Some({
                        let mut yielded = JitterPacket {
                            raw: packet.clone(),
                            sequence_number: packet
                                .as_ref()
                                .map(|p| p.sequence_number())
                                .unwrap_or(u16::from(last.sequence_number).wrapping_add(1))
                                .into(),
                            yielded_at: Some(SystemTime::now()),
                        };
                        yielded.yielded_at = Some(SystemTime::now());
                        yielded
                    });

                    #[cfg(feature = "log")]
                    log::debug!(
                        "packet {:?} yielded after delay, {} remaining",
                        self.last.as_ref().map(|l| l.sequence_number),
                        self.heap.len()
                    );

                    Poll::Ready(packet)
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // TODO: Verify
                // check if we have enough packets in the jitter to fight network jitter
                // this amount should be calcualted based on network latency! find an algorithm for
                // delaying playback!

                let buffered_samples: usize = self
                    .heap
                    .iter()
                    .map(|p| p.raw.as_ref().map(|raw| raw.samples()).unwrap_or(0))
                    .sum();

                if (buffered_samples as f32 / self.sample_rate as f32)
                    < (Self::MAX_DELAY.as_secs_f32() * self.plr())
                {
                    if let Some(ref producer) = self.producer {
                        producer.wake_by_ref();
                    }
                }

                let estimated_samples_of_packet = match last.raw.as_ref() {
                    Some(raw) => raw.samples(),
                    None => match self.heap.peek() {
                        Some(packet) => match packet.raw.as_ref() {
                            Some(raw) => raw.samples(),
                            None => 0,
                        },
                        None => 0,
                    },
                };

                let samples = estimated_samples_of_packet / self.channels;
                let fraction = samples as f32 / self.sample_rate as f32;
                let elapsed = last
                    .yielded_at
                    .unwrap_or_else(SystemTime::now)
                    .elapsed()
                    .unwrap_or(Duration::ZERO);
                let duration =
                    Duration::from_millis((fraction * 1000.0f32) as u64).saturating_sub(elapsed);

                self.delay = Some(Delay::new(duration));

                let _ = self.delay.as_mut().unwrap().poll_unpin(cx);

                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.heap.len(), None)
    }
}

pub trait Packet: Unpin + Clone {
    fn sequence_number(&self) -> u16;
    fn offset(&self) -> usize;
    fn samples(&self) -> usize;
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
    use futures::SinkExt;
    use futures::{executor::block_on, StreamExt};
    use std::time::SystemTime;

    const SAMPLE_RATE: usize = 48000;
    const CHANNELS: usize = 2;

    #[derive(Debug, Clone, PartialEq)]
    struct RTP {
        seq: u16,
        offset: usize,
    }

    impl Packet for RTP {
        #[inline]
        fn sequence_number(&self) -> u16 {
            self.seq
        }

        #[inline]
        fn offset(&self) -> usize {
            self.offset
        }

        #[inline]
        fn samples(&self) -> usize {
            960
        }
    }

    #[test]
    fn const_capacity() {
        let jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);
        assert_eq!(jitter.heap.capacity(), 10);
    }

    #[test]
    fn send() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);
        let packet = RTP { seq: 0, offset: 0 };
        block_on(jitter.send(packet.clone())).unwrap();
        assert_eq!(jitter.heap.peek(), Some(&packet.into()));
    }

    #[test]
    fn playback_according_to_sample_rate() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP {
            seq: 1,
            offset: 960,
        }))
        .unwrap();
        block_on(jitter.send(RTP {
            seq: 2,
            offset: 960 * 2,
        }))
        .unwrap();

        assert_eq!(jitter.heap.len(), 3);
        assert!(jitter.last.is_none());

        let start = SystemTime::now();

        assert_eq!(block_on(jitter.next()), Some(RTP { seq: 0, offset: 0 }));
        assert_eq!(start.elapsed().unwrap().subsec_millis(), 0);
        assert_eq!(jitter.heap.len(), 2);
        assert_eq!(
            jitter
                .last
                .as_ref()
                .unwrap()
                .raw
                .as_ref()
                .unwrap()
                .sequence_number(),
            0
        );
        assert_eq!(
            jitter.last.as_ref().unwrap().raw.as_ref().unwrap().offset(),
            0
        );

        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 1,
                offset: 960
            })
        );
        assert_eq!(start.elapsed().unwrap().subsec_millis(), 10);
        assert_eq!(jitter.heap.len(), 1);
        assert_eq!(
            jitter
                .last
                .as_ref()
                .unwrap()
                .raw
                .as_ref()
                .unwrap()
                .sequence_number(),
            1
        );
        assert_eq!(
            jitter.last.as_ref().unwrap().raw.as_ref().unwrap().offset(),
            960
        );

        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 2,
                offset: 960 * 2
            })
        );
        assert_eq!(start.elapsed().unwrap().subsec_millis(), 20);

        assert_eq!(jitter.heap.len(), 0);
        assert_eq!(
            jitter
                .last
                .as_ref()
                .unwrap()
                .raw
                .as_ref()
                .unwrap()
                .sequence_number(),
            2
        );
        assert_eq!(
            jitter.last.as_ref().unwrap().raw.as_ref().unwrap().offset(),
            960 * 2
        );
    }

    #[test]
    fn reorders_racing_packets() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        assert_eq!(block_on(jitter.next()), Some(RTP { seq: 0, offset: 0 }));

        block_on(jitter.send(RTP {
            seq: 2,
            offset: 960 * 2,
        }))
        .unwrap();

        block_on(jitter.send(RTP {
            seq: 1,
            offset: 960,
        }))
        .unwrap();

        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 1,
                offset: 960
            })
        );

        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 2,
                offset: 960 * 2
            })
        );
    }

    #[test]
    fn discards_already_played_packets() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        assert_eq!(block_on(jitter.next()), Some(RTP { seq: 0, offset: 0 }));

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();

        block_on(jitter.send(RTP {
            seq: 1,
            offset: 960,
        }))
        .unwrap();
        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 1,
                offset: 960
            })
        );
    }

    #[test]
    fn discards_duplicated_packets() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();

        assert_eq!(block_on(jitter.next()), Some(RTP { seq: 0, offset: 0 }));
        assert_eq!(jitter.heap.len(), 0);
    }

    #[test]
    fn handles_packet_loss_correctly() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        block_on(jitter.send(RTP { seq: 0, offset: 0 })).unwrap();
        block_on(jitter.send(RTP {
            seq: 1,
            offset: 960,
        }))
        .unwrap();
        block_on(jitter.send(RTP {
            seq: 2,
            offset: 960 * 2,
        }))
        .unwrap();
        block_on(jitter.send(RTP {
            seq: 3,
            offset: 960 * 3,
        }))
        .unwrap();
        block_on(jitter.send(RTP {
            seq: 5,
            offset: 960 * 5,
        }))
        .unwrap();

        assert_eq!(block_on(jitter.next()), Some(RTP { seq: 0, offset: 0 }));
        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 1,
                offset: 960
            })
        );
        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 2,
                offset: 960 * 2
            })
        );
        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 3,
                offset: 960 * 3
            })
        );
        assert_eq!(block_on(jitter.next()), None);
        assert_eq!(
            block_on(jitter.next()),
            Some(RTP {
                seq: 5,
                offset: 960 * 5
            })
        );
    }

    #[test]
    fn handles_wrapping_sequence_numbers() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(SAMPLE_RATE, CHANNELS);

        let rtp = |seq, logical_seq: usize| RTP {
            seq,
            offset: 960 * logical_seq,
        };

        let pop_seq =
            |jitter: &mut JitterBuffer<RTP, 10>| jitter.heap.pop().unwrap().sequence_number.0;

        block_on(jitter.send(rtp(u16::MAX - 2, 0))).unwrap();
        block_on(jitter.send(rtp(u16::MAX - 1, 1))).unwrap();
        block_on(jitter.send(rtp(u16::MAX, 2))).unwrap();
        block_on(jitter.send(rtp(u16::MIN, 3))).unwrap();
        block_on(jitter.send(rtp(u16::MIN + 1, 4))).unwrap();
        block_on(jitter.send(rtp(u16::MIN + 2, 5))).unwrap();

        assert_eq!(jitter.heap.len(), 6);
        assert_eq!(pop_seq(&mut jitter), u16::MAX - 2);
        assert_eq!(pop_seq(&mut jitter), u16::MAX - 1);
        assert_eq!(pop_seq(&mut jitter), u16::MAX);
        assert_eq!(pop_seq(&mut jitter), u16::MIN);
        assert_eq!(pop_seq(&mut jitter), u16::MIN + 1);
        assert_eq!(pop_seq(&mut jitter), u16::MIN + 2);
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
