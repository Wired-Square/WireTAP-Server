//! A serial line's bytes as the reads returned them, unframed.

use std::time::SystemTime;

use wiretap_model::{SerialSample, SourceId};
use wslib_serial::LineSettings;
use wslib_wiretap_protocol::ingest::{self, RawChunk};

use super::system_time_to_us;

/// One serial line's raw capture, and the bus number its chunks carry.
pub struct RawTap {
    tap: ingest::RawTap,
    bus: SourceId,
}

impl RawTap {
    pub fn new(bus: SourceId, line: LineSettings) -> Self {
        Self {
            tap: ingest::RawTap::new(line.baud, line.bits_per_char()),
            bus,
        }
    }

    /// One read's bytes in chunks a record can carry, each stamped when its
    /// last byte arrived.
    pub fn push(&mut self, bytes: &[u8], read_at: SystemTime) -> Vec<SerialSample> {
        self.tap
            .push(bytes, read_at)
            .into_iter()
            .map(|RawChunk { at, seq, data }| SerialSample {
                ts_us: system_time_to_us(at),
                bus: self.bus,
                seq,
                data,
            })
            .collect()
    }

    /// The line was opened again: its reads count from zero.
    pub fn reset(&mut self) {
        self.tap.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use wslib_serial::Parity;

    use super::*;

    const LINE_9600_8N1: LineSettings = LineSettings {
        baud: 9600,
        data_bits: 8,
        parity: Parity::None,
        stop_bits: 1,
    };

    const T0: i64 = 1_700_000_000_000_000;

    fn at(us: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_micros(us as u64)
    }

    #[test]
    fn a_long_read_is_chunked_and_each_chunk_stamped_by_its_last_byte() {
        let bytes: Vec<u8> = (0..600).map(|i| i as u8).collect();
        let chunks = RawTap::new(SourceId(4), LINE_9600_8N1).push(&bytes, at(T0));

        let lens: Vec<usize> = chunks.iter().map(|c| c.data.len()).collect();
        assert_eq!(lens, [256, 256, 88]);
        assert_eq!(
            chunks
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>(),
            bytes
        );
        // 10 bits a byte at 9600 baud, truncated to whole microseconds.
        let stamps: Vec<i64> = chunks.iter().map(|c| c.ts_us).collect();
        assert_eq!(stamps, [T0 - 358_333, T0 - 91_666, T0]);
        assert!(chunks.iter().all(|c| c.bus == SourceId(4) && c.seq == 0));
    }

    #[test]
    fn the_sequence_counts_reads_from_the_open() {
        let mut tap = RawTap::new(SourceId(0), LINE_9600_8N1);
        let seqs = |tap: &mut RawTap, len: usize| -> Vec<u32> {
            tap.push(&vec![0; len], at(T0))
                .iter()
                .map(|c| c.seq)
                .collect()
        };
        assert_eq!(seqs(&mut tap, 1), [0]);
        assert_eq!(seqs(&mut tap, 300), [1, 1], "chunks of one read share it");
        assert_eq!(seqs(&mut tap, 1), [2]);

        tap.reset();
        assert_eq!(seqs(&mut tap, 1), [0], "counted again from a reopen");
    }

    #[test]
    fn wire_time_reaching_before_the_epoch_stamps_the_epoch() {
        let chunks = RawTap::new(SourceId(0), LINE_9600_8N1).push(&[0; 300], at(1_000));
        assert_eq!(chunks[0].ts_us, 0);
    }
}
