use futures::sink::Sink;
use futures::{FutureExt, Stream};
use futures_timer::Delay;
use std::collections::BinaryHeap;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

const SAMPLE_RATE: usize = 48000;

#[derive(Debug)]
pub struct JitterBuffer<P, const S: usize>
where
    P: Packet,
{
    last: Option<JitterHeader>,
    delay: Option<Delay>,

    queued: Option<P>,
    heap: BinaryHeap<JitterPacket<P>>,

    producer: Option<Waker>,
    consumer: Option<Waker>,

    #[doc(hidden)]
    // Prevents autoimplementation of send and sync
    raw: PhantomData<*mut ()>,
}

impl<P, const S: usize> JitterBuffer<P, S>
where
    P: Packet,
{
    pub fn new() -> Self {
        Self {
            last: None,
            delay: None,

            queued: None,
            heap: BinaryHeap::with_capacity(S),

            producer: None,
            consumer: None,

            raw: PhantomData::default(),
        }
    }
}

impl<P, const S: usize> Sink<P> for JitterBuffer<P, S>
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
            if self.heap.len() >= S {
                self.queued = Some(packet);
                self.producer = Some(cx.waker().clone());
                return Poll::Pending;
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

impl<P, const S: usize> Stream for JitterBuffer<P, S>
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

        // check if we have enough packets in the jitter to fight network jitter
        // this amount should be calcualted based on network latency! find an algorithm for
        // delaying playback!

        let JitterHeader {
            sequence_number,
            offset,
            samples,
            yielded_at,
        } = match self.last {
            Some(ref last) => last,
            // no need to delay until the last packet is played back since
            // we are yielding the first packet right now
            None => {
                // SAFETY:
                // we checked that the heap is not empty so at least one
                // element must be present or the std implementation is flawed.
                let packet = self.heap.pop().unwrap().into();
                self.last = Some(JitterHeader::new(&packet, SystemTime::now()));

                println!("yielding first packet: sn {}", packet.sequence_number());

                return Poll::Ready(Some(packet));
            }
        };

        println!(
            "we have last: sn {} with offset {} and samples {} yielded at {:?}",
            sequence_number,
            offset,
            samples,
            yielded_at.elapsed().unwrap()
        );

        // we handed a packet before, lets sleep if it is played back completly
        match self.delay.as_mut() {
            Some(ref mut delay) => match delay.poll_unpin(cx) {
                Poll::Ready(_) => {
                    self.delay = None;

                    let packet = match self.heap.pop() {
                        Some(packet) => packet.into(),
                        None => return Poll::Pending,
                    };

                    self.last = Some(JitterHeader::new(&packet, SystemTime::now()));

                    println!(
                        "yieleded after delay resolved: sn {}",
                        packet.sequence_number()
                    );

                    Poll::Ready(Some(packet))
                }
                Poll::Pending => Poll::Pending,
            },
            None => {
                // calculate packet duration based on sample length

                let mut delay = Delay::new(Duration::from_millis(20));
                assert!(delay.poll_unpin(cx).is_pending());
                self.delay = Some(delay);
                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.heap.len(), None)
    }
}

pub trait Packet: Unpin {
    fn sequence_number(&self) -> usize;
    fn offset(&self) -> usize;
    fn samples(&self) -> usize;
}

#[derive(Debug)]
pub(crate) struct JitterPacket<P>
where
    P: Packet,
{
    pub(crate) raw: P,
}

impl<P> JitterPacket<P>
where
    P: Packet,
{
    fn into(self) -> P {
        self.raw
    }
}

impl<P> From<P> for JitterPacket<P>
where
    P: Packet,
{
    fn from(raw: P) -> Self {
        Self { raw }
    }
}

impl<P> PartialEq for JitterPacket<P>
where
    P: Packet,
{
    fn eq(&self, other: &Self) -> bool {
        self.raw.sequence_number().eq(&other.raw.sequence_number())
    }
}

impl<P> Eq for JitterPacket<P> where P: Packet {}

impl<P> PartialOrd for JitterPacket<P>
where
    P: Packet,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.raw
            .sequence_number()
            .partial_cmp(&other.raw.sequence_number())
            .map(|ordering| ordering.reverse())
    }
}

impl<P> Ord for JitterPacket<P>
where
    P: Packet,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw
            .sequence_number()
            .cmp(&other.raw.sequence_number())
            .reverse()
    }
}

#[derive(Debug)]
struct JitterHeader {
    yielded_at: SystemTime,
    sequence_number: usize,
    offset: usize,
    samples: usize,
}

impl JitterHeader {
    pub fn new(packet: &impl Packet, yielded_at: SystemTime) -> Self {
        JitterHeader {
            yielded_at,
            sequence_number: packet.sequence_number(),
            offset: packet.offset(),
            samples: packet.samples(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use std::time::SystemTime;

    #[derive(Debug, Clone, PartialEq)]
    struct RTP {
        seq_num: u16,
    }

    impl Packet for RTP {
        #[inline]
        fn span(&self) -> Duration {
            Duration::from_millis(20)
        }

        #[inline]
        fn seq_num(&self) -> u16 {
            self.seq_num
        }
    }

    #[test]
    fn const_capacity() {
        let jitter = JitterBuffer::<RTP, 10>::new(48000);
        assert_eq!(jitter.heap.capacity(), 10);
    }

    #[test]
    fn send() {
        let mut jitter = JitterBuffer::<RTP, 10>::new(48000);

        let packet = RTP { seq_num: 0 };

        let before = SystemTime::now();
        futures::executor::block_on(jitter.send(packet.clone())).unwrap();
        assert!(before.elapsed().unwrap() < Duration::from_micros(10));

        assert_eq!(jitter.heap.peek(), Some(&packet.into()));
    }
}
