use futures::sink::Sink;
use futures::Stream;
use std::collections::BinaryHeap;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Debug)]
pub struct JitterBuffer<P, const S: usize>
where
    P: Packet,
{
    offset: usize,
    hz: usize,

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
    pub fn new(hz: usize) -> Self {
        Self {
            offset: 0,
            hz,

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
                return Poll::Pending;
            }

            self.heap.push(JitterPacket(packet));

            self.waker.iter().for_each(|w| w.wake_by_ref());
            cx.waker().wake_by_ref();
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

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.heap.is_empty() {
            return Poll::Pending;
        }

        //if let Some(ref last) = self.last_played {
        //let lasted = last.play_back_at() + last.span();
        //let next = self.heap.peek();

        //if lasted <= SystemTime::now() && next.play_back_at() <= SystemTime::now() {
        //self.last_played.replace(next.clone());
        //self.buffer.splice
        //}
        //}

        Poll::Pending
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.heap.len(), None)
    }
}

pub trait Packet: Unpin {
    fn seq_num(&self) -> u16;
    fn play_back_at(&self) -> SystemTime;
    fn received_at(&self) -> SystemTime;
    fn span(&self) -> Duration;
}

#[derive(Debug)]
pub(crate) struct JitterPacket<P>(pub(crate) P)
where
    P: Packet;

impl<P> PartialEq for JitterPacket<P>
where
    P: Packet,
{
    fn eq(&self, other: &Self) -> bool {
        self.0.seq_num().eq(&other.0.seq_num())
    }
}

impl<P> Eq for JitterPacket<P> where P: Packet {}

impl<P> PartialOrd for JitterPacket<P>
where
    P: Packet,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.seq_num().partial_cmp(&other.0.seq_num())
    }
}

impl<P> Ord for JitterPacket<P>
where
    P: Packet,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.seq_num().cmp(&other.0.seq_num())
    }
}

#[cfg(feature = "rtp")]
impl Packet for rtp::packet::Packet {
    #[inline]
    fn seq_num(&self) -> u16 {
        self.header.sequence_number
    }

    #[inline]
    fn received_at(&self) -> SystemTime {
        SystemTime::now()
    }

    #[inline]
    fn play_back_at(&self) -> SystemTime {
        SystemTime::now()
    }

    #[inline]
    fn span(&self) -> Duration {
        Duration::from_millis(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;

    #[derive(Debug, Clone, PartialEq)]
    struct RTP {
        seq_num: u16,
        play_back_at: SystemTime,
        received_at: SystemTime,
    }

    impl Packet for RTP {
        #[inline]
        fn span(&self) -> Duration {
            Duration::from_millis(10)
        }

        #[inline]
        fn seq_num(&self) -> u16 {
            self.seq_num
        }

        #[inline]
        fn play_back_at(&self) -> SystemTime {
            self.play_back_at
        }

        #[inline]
        fn received_at(&self) -> SystemTime {
            self.received_at
        }
    }

    #[test]
    fn const_capacity() {
        let jitter = JitterBuffer::<RTP, 10>::new();
        assert_eq!(jitter.heap.capacity(), 10);
    }

    #[test]
    fn send() {
        let mut jitter = JitterBuffer::<RTP, 10>::new();

        let packet = RTP {
            seq_num: 0,
            play_back_at: SystemTime::now(),
            received_at: SystemTime::now(),
        };

        let before = SystemTime::now();
        futures::executor::block_on(jitter.send(packet.clone())).unwrap();
        assert!(before.elapsed().unwrap() < Duration::from_micros(10));

        assert_eq!(jitter.heap.peek(), Some(&JitterPacket(packet)));
    }
}
