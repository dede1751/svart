use crate::definitions::{NOMOVE, TB_LOSS_IN_PLY, TB_WIN_IN_PLY};

use cozy_chess::{Move, Piece, Square};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TTFlag {
    None,
    Exact,
    LowerBound,
    UpperBound,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackedMove(u16);

impl PackedMove {
    pub fn new(mv: Option<Move>) -> Self {
        if mv.is_none() {
            return Self(NOMOVE);
        }

        let mv = mv.unwrap();
        let from = mv.from as u16; // 0..63, 6 bits
        let to = mv.to as u16; // 0..63, 6 bits

        // First bit represents promotion, next 2 bits represent piece type
        let promotion: u16 = match mv.promotion {
            None => 0b000,
            Some(Piece::Knight) => 0b100,
            Some(Piece::Bishop) => 0b101,
            Some(Piece::Rook) => 0b110,
            Some(Piece::Queen) => 0b111,
            _ => unreachable!(),
        };

        // 6 + 6 + 3 bits and one for padding gets a 2 byte move
        let packed = from | to << 6 | promotion << 12;

        Self(packed)
    }

    pub fn unpack(self) -> Move {
        let from = Square::index((self.0 & 0b111111) as usize);
        let to = Square::index(((self.0 >> 6) & 0b111111) as usize);

        let promotion = match (self.0 >> 12) & 0b111 {
            0b000 => None,
            0b100 => Some(Piece::Knight),
            0b101 => Some(Piece::Bishop),
            0b110 => Some(Piece::Rook),
            0b111 => Some(Piece::Queen),
            _ => unreachable!(),
        };

        Move {
            from,
            to,
            promotion,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgeAndFlag(pub u8);

impl AgeAndFlag {
    fn new(age: u8, flag: TTFlag) -> Self {
        let flag = match flag {
            TTFlag::None => 0b00,
            TTFlag::Exact => 0b01,
            TTFlag::LowerBound => 0b10,
            TTFlag::UpperBound => 0b11,
        };

        Self(age << 2 | flag)
    }

    fn age(&self) -> u8 {
        self.0 >> 2
    }

    pub fn flag(&self) -> TTFlag {
        match self.0 & 0b11 {
            0b00 => TTFlag::None,
            0b01 => TTFlag::Exact,
            0b10 => TTFlag::LowerBound,
            0b11 => TTFlag::UpperBound,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TTEntry {
    pub mv: PackedMove,       // 2 byte move wrapper (6 sq + 6 sq + 3 promo bits)
    pub key: u16,             // 2 bytes
    pub score: i16,           // 2 bytes
    pub depth: u8,            // 1 byte
    pub age_flag: AgeAndFlag, // 1 byte wrapper (6 age + 2 flag bits)
}

impl TTEntry {
    #[must_use]
    fn quality(&self) -> u16 {
        let age = self.age_flag.age();
        (age * 2 + self.depth) as u16
    }
}

// Thank you to Spamdrew and Cosmo for help in implementing the atomic TT
impl From<u64> for TTEntry {
    fn from(data: u64) -> Self {
        // SAFETY: This is safe because all fields of TTEntry are (at base) integral types, and order is known.
        unsafe { std::mem::transmute(data) }
    }
}

impl From<TTEntry> for u64 {
    fn from(entry: TTEntry) -> Self {
        // SAFETY: This is safe because all bitpatterns of `u64` are valid.
        unsafe { std::mem::transmute(entry) }
    }
}

pub struct TT {
    pub entries: Vec<AtomicU64>,
    pub epoch: u8,
}

impl TT {
    pub fn new(mb: u32) -> Self {
        let hash_size = mb * 1024 * 1024;
        let size = hash_size / std::mem::size_of::<TTEntry>() as u32;
        let mut entries = Vec::with_capacity(size as usize);

        for _ in 0..size {
            entries.push(AtomicU64::new(0));
        }

        Self { entries, epoch: 0 }
    }

    #[must_use]
    pub fn index(&self, key: u64) -> usize {
        // Cool hack Cosmo taught me
        let key = key as u128;
        let len = self.entries.len() as u128;
        ((key * len) >> 64) as usize
    }

    #[must_use]
    pub fn probe(&self, key: u64) -> TTEntry {
        let atomic = &self.entries[self.index(key)];
        let entry = atomic.load(Ordering::Relaxed);

        TTEntry::from(entry)
    }

    pub fn age(&mut self) {
        // Cap at 63 for wrapping into 6 bits
        const EPOCH_MAX: u8 = 63;

        if self.epoch == EPOCH_MAX {
            self.epoch = 0;

            self.entries.iter_mut().for_each(|a| {
                let entry = a.load(Ordering::Relaxed);
                let mut entry = TTEntry::from(entry);

                entry.age_flag = AgeAndFlag::new(0, entry.age_flag.flag());

                a.store(entry.into(), Ordering::Relaxed);
            })
        }

        self.epoch += 1;
    }

    pub fn store(
        &self,
        key: u64,
        mv: Option<Move>,
        score: i16,
        depth: u8,
        flag: TTFlag,
        ply: usize,
    ) {
        let target_index = self.index(key);
        let target_atomic = &self.entries[target_index];
        let mut target: TTEntry = target_atomic.load(Ordering::Relaxed).into();

        let entry = TTEntry {
            key: key as u16,
            mv: PackedMove::new(mv),
            score: score_to_tt(score, ply),
            depth,
            age_flag: AgeAndFlag::new(self.epoch, flag),
        };

        // Only replace entries of similar or higher quality
        if entry.quality() >= target.quality() {
            let positions_differ = target.key != entry.key;

            target.key = entry.key;
            target.score = entry.score;
            target.depth = entry.depth;
            target.age_flag = entry.age_flag;

            // Do not overwrite the move if there was no new best move
            if mv.is_some() || positions_differ {
                target.mv = entry.mv;
            }

            target_atomic.store(target.into(), Ordering::Relaxed);
        }
    }

    pub fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

            let index = self.index(key);
            let entry = &self.entries[index];

            _mm_prefetch((entry as *const AtomicU64).cast::<i8>(), _MM_HINT_T0);
        }
    }

    pub fn reset(&mut self) {
        self.entries.iter().for_each(|a| {
            a.store(0, Ordering::Relaxed);
        })
    }
}

#[must_use]
pub fn score_to_tt(score: i16, ply: usize) -> i16 {
    if score >= TB_WIN_IN_PLY as i16 {
        score + ply as i16
    } else if score <= TB_LOSS_IN_PLY as i16 {
        score - ply as i16
    } else {
        score
    }
}

#[must_use]
pub fn score_from_tt(score: i16, ply: usize) -> i16 {
    if score >= TB_WIN_IN_PLY as i16 {
        score - ply as i16
    } else if score <= TB_LOSS_IN_PLY as i16 {
        score + ply as i16
    } else {
        score
    }
}

const _TT_TEST: () = assert!(std::mem::size_of::<TTEntry>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tt_reset() {
        let mut tt = TT::new(1);
        let mv = Move {
            from: Square::A1,
            to: Square::A2,
            promotion: None,
        };
        tt.store(5, Some(mv), 1, 3, TTFlag::UpperBound, 22);
        assert_eq!(tt.probe(5).score, 1);

        tt.reset();
        tt.entries.iter().for_each(|e| {
            let e = e.load(Ordering::Relaxed);
            let e = TTEntry::from(e);

            assert_eq!(e.score, 0);
            assert_eq!(e.age_flag, AgeAndFlag(0));
            assert_eq!(e.depth, 0);
            assert_eq!(e.key, 0);
            assert_eq!(e.mv, PackedMove(NOMOVE));
        });
    }

    #[test]
    fn packed_moves() {
        let mv = Move {
            from: Square::A1,
            to: Square::A2,
            promotion: None,
        };
        let packed = PackedMove::new(Some(mv));
        assert_eq!(packed.unpack(), mv);

        let mv = Move {
            from: Square::B7,
            to: Square::A2,
            promotion: Some(Piece::Knight),
        };
        let packed = PackedMove::new(Some(mv));
        assert_eq!(packed.unpack(), mv);

        let mv = Move {
            from: Square::C1,
            to: Square::A2,
            promotion: Some(Piece::Bishop),
        };
        let packed = PackedMove::new(Some(mv));
        assert_eq!(packed.unpack(), mv);

        let mv = Move {
            from: Square::H3,
            to: Square::H4,
            promotion: Some(Piece::Rook),
        };
        let packed = PackedMove::new(Some(mv));
        assert_eq!(packed.unpack(), mv);

        let mv = Move {
            from: Square::D8,
            to: Square::D7,
            promotion: Some(Piece::Queen),
        };
        let packed = PackedMove::new(Some(mv));
        assert_eq!(packed.unpack(), mv);
    }

    #[test]
    fn age_flag() {
        let entry = TTEntry {
            key: 0,
            mv: PackedMove(NOMOVE),
            score: 0,
            depth: 0,
            age_flag: AgeAndFlag::new(5, TTFlag::Exact),
        };

        assert_eq!(entry.age_flag.age(), 0b0000_0101);
        assert_eq!(entry.age_flag.age(), 5);
        assert_eq!(entry.age_flag.flag(), TTFlag::Exact);

        let entry = TTEntry {
            key: 0,
            mv: PackedMove(NOMOVE),
            score: 0,
            depth: 0,
            age_flag: AgeAndFlag::new(63, TTFlag::UpperBound),
        };

        assert_eq!(entry.age_flag.age(), 0b0011_1111);
        assert_eq!(entry.age_flag.age(), 63);
        assert_eq!(entry.age_flag.flag(), TTFlag::UpperBound);

        let entry = TTEntry {
            key: 0,
            mv: PackedMove(NOMOVE),
            score: 0,
            depth: 0,
            age_flag: AgeAndFlag::new(0, TTFlag::LowerBound),
        };

        assert_eq!(entry.age_flag.age(), 0b0000_0000);
        assert_eq!(entry.age_flag.age(), 0);
        assert_eq!(entry.age_flag.flag(), TTFlag::LowerBound);
    }
}
