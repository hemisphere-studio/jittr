use std::time;

#[derive(Debug)]
pub struct JitterBuffer<P, const SIZE: usize>
where
    P: Packet,
{
    last_played: Option<P>,
    buffer: Vec<P>,
}

impl<P, const SIZE: usize> JitterBuffer<P, SIZE>
where
    P: Packet,
{
    pub fn new() -> Self {
        Self {
            last_played: None,
            buffer: Vec::with_capacity(SIZE),
        }
    }
}

pub trait Packet {
    fn seq_num(&self) -> u16;
    fn play_back_at(&self) -> time::SystemTime;
    fn received_at(&self) -> time::SystemTime;
    fn span(&self) -> time::Duration;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct RTP {
        seq_num: u16,
        play_back_at: time::SystemTime,
        received_at: time::SystemTime,
    }

    impl Packet for RTP {
        #[inline]
        fn span(&self) -> time::Duration {
            time::Duration::from_millis(10)
        }

        #[inline]
        fn seq_num(&self) -> u16 {
            self.seq_num
        }

        #[inline]
        fn play_back_at(&self) -> time::SystemTime {
            self.play_back_at
        }

        #[inline]
        fn received_at(&self) -> time::SystemTime {
            self.received_at
        }
    }

    #[test]
    fn size() {
        let jitter = JitterBuffer::<RTP, 10>::new();
        assert_eq!(jitter.buffer.capacity(), 10);
    }
}
