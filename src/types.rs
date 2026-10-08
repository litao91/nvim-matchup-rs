//! Core runtime types: positions, delimiters, matching lists.

use std::collections::{BTreeMap, HashMap};

use crate::words::{Grp, Side, WordId};

/// Sentinel word id for delims in a matching list that are mids
/// (vim uses the string '__mid__').
pub const MID_SENTINEL: WordId = usize::MAX;

/// 1-based line number, 1-based byte column (vim conventions).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pos {
    pub lnum: usize,
    pub cnum: usize,
}

impl Pos {
    pub fn new(lnum: usize, cnum: usize) -> Pos {
        Pos { lnum, cnum }
    }

    /// Port of matchup#pos#val (pos.vim:42).
    pub fn val(&self) -> i64 {
        100000 * self.lnum as i64 + self.cnum.min(90000) as i64
    }

    pub fn smaller(&self, other: &Pos) -> bool {
        self.val() < other.val()
    }

    pub fn larger(&self, other: &Pos) -> bool {
        self.val() > other.val()
    }
}

/// Port of matchup#pos#next (pos.vim:64): next character position,
/// wrapping to the next line past end-of-line.
pub fn pos_next(line: &str, p: Pos) -> Pos {
    let i = p.cnum.checked_sub(1).unwrap_or(0);
    if i < line.len() {
        let mut j = i;
        while j > 0 && !line.is_char_boundary(j) {
            j -= 1;
        }
        if let Some(c) = line[j..].chars().next() {
            let charlen = c.len_utf8();
            if p.cnum + charlen <= line.len() {
                return Pos::new(p.lnum, p.cnum + charlen);
            }
        }
    }
    Pos::new(p.lnum + 1, 1)
}

/// Port of matchup#pos#prev (pos.vim:88): previous character position.
/// `line` is the text of `p.lnum`, `prev_line` the text of `p.lnum - 1`.
pub fn pos_prev(line: &str, prev_line: &str, p: Pos) -> Pos {
    if p.cnum > 1 {
        let prefix_end = (p.cnum - 1).min(line.len());
        let mut k = prefix_end;
        while k > 0 && !line.is_char_boundary(k) {
            k -= 1;
        }
        if k > 0 {
            if let Some(c) = line[..k].chars().next_back() {
                k -= c.len_utf8();
            }
        }
        Pos::new(p.lnum, k + 1)
    } else {
        Pos::new((p.lnum - 1).max(1), prev_line.len().max(1))
    }
}

/// A located delimiter (port of vim-matchup's delim dict).
#[derive(Clone, Debug)]
pub struct Delim {
    pub lnum: usize,
    pub cnum: usize,
    /// The matched text (match extent).
    pub match_: String,
    pub side: Side,
    /// Index into the delimiter sets.
    pub set: usize,
    /// Word id within the set: 0 = open, 1..n = mids, n+1 = close,
    /// MID_SENTINEL for matching-list entries.
    pub word_id: WordId,
    /// Skip state at the match.
    pub skip: bool,
    /// Captured groups indexed by open-pattern backref number.
    pub groups: HashMap<Grp, String>,
    /// Partially resolved open pattern for upward searches ('' = none).
    pub augment_str: String,
    pub augment_unresolved: BTreeMap<Grp, Grp>,
    pub highlighting: bool,
    pub match_index: usize,
    /// Treesitter engine cache id (0 = classic engine delim).
    pub ts_id: u64,
}

impl Delim {
    pub fn pos(&self) -> Pos {
        Pos::new(self.lnum, self.cnum)
    }

    /// Port of matchup#delim#end_offset (delim.vim:313): byte index of
    /// the last character of the match.
    pub fn end_offset(&self) -> usize {
        match self.match_.chars().next_back() {
            Some(c) => self.match_.len() - c.len_utf8(),
            None => 0,
        }
    }

    pub fn end_pos(&self) -> Pos {
        Pos::new(self.lnum, self.cnum + self.end_offset())
    }
}

/// The list of delimiters forming one complete match group
/// (open, mids..., close) with circular links, port of the
/// matching_list built by matchup#delim#get_matching (delim.vim:101-146).
#[derive(Clone, Debug, Default)]
pub struct MatchingList {
    pub delims: Vec<Delim>,
    pub next: Vec<usize>,
    pub prev: Vec<usize>,
}

impl MatchingList {
    pub fn len(&self) -> usize {
        self.delims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.delims.is_empty()
    }

    pub fn open(&self) -> &Delim {
        &self.delims[0]
    }

    pub fn close(&self) -> &Delim {
        self.delims.last().unwrap()
    }

    /// Index of the seed delim's entry: list entries built from scan
    /// results carry the MID_SENTINEL word_id, while the seed entry is a
    /// clone of the original delim and keeps its real word_id.
    pub fn seed_index(&self) -> usize {
        self.delims
            .iter()
            .position(|d| d.word_id != MID_SENTINEL)
            .unwrap_or(0)
    }

    /// Link target after sentinel adjustment (delim.vim:138-144).
    pub fn next_of(&self, i: usize) -> usize {
        self.next[i]
    }

    pub fn prev_of(&self, i: usize) -> usize {
        self.prev[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_val_ordering() {
        assert!(Pos::new(10, 5).smaller(&Pos::new(11, 1)));
        assert!(Pos::new(10, 5).larger(&Pos::new(10, 4)));
        assert_eq!(Pos::new(1, 1).val(), 100001);
    }

    #[test]
    fn next_prev_ascii() {
        assert_eq!(pos_next("abcdef", Pos::new(3, 2)), Pos::new(3, 3));
        // past EOL wraps
        assert_eq!(pos_next("ab", Pos::new(3, 2)), Pos::new(4, 1));
        assert_eq!(pos_prev("abcdef", "", Pos::new(3, 3)), Pos::new(3, 2));
        // prev at col 1 goes to previous line's strlen
        assert_eq!(pos_prev("x", "abcdef", Pos::new(3, 1)), Pos::new(2, 6));
        assert_eq!(pos_prev("x", "", Pos::new(1, 1)), Pos::new(1, 1));
    }

    #[test]
    fn next_prev_multibyte() {
        // "é" is 2 bytes
        assert_eq!(pos_next("éx", Pos::new(1, 1)), Pos::new(1, 3));
        assert_eq!(pos_prev("éx", "", Pos::new(1, 3)), Pos::new(1, 1));
    }

    #[test]
    fn end_offset_works() {
        let d = Delim {
            lnum: 1,
            cnum: 1,
            match_: "abc".to_string(),
            side: Side::Open,
            set: 0,
            word_id: 0,
            skip: false,
            groups: HashMap::new(),
            augment_str: String::new(),
            augment_unresolved: BTreeMap::new(),
            highlighting: false,
            match_index: 0,
            ts_id: 0,
        };
        assert_eq!(d.end_offset(), 2);
        assert_eq!(d.end_pos(), Pos::new(1, 3));
        let mut e = d.clone();
        e.match_ = "é".to_string();
        assert_eq!(e.end_offset(), 0); // single char, byte index 0
        e.match_ = "".to_string();
        assert_eq!(e.end_offset(), 0);
    }
}
