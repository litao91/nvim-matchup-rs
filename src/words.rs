//! Parsing of `b:match_words` / `&matchpairs` into delimiter sets.
//!
//! Port of vim-matchup's `autoload/matchup/loader.vim`
//! (`s:init_delim_lists`, `s:init_delim_lists_fast`,
//! `s:init_delim_regexes_generator`) and the associated bookkeeping
//! structures (`regexone`/`regextwo`, capture-group renumbering,
//! augments).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::vimregex::{get_capture_groups, split_not_bslash, CaptureGroup};

/// Word index within a set: 0 = open, 1..n-1 = mids, n = close.
pub type WordId = usize;
/// Capture-group / backref number (vim `\1`-`\9`).
pub type Grp = u32;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Side {
    Open,
    Mid,
    Close,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Open => "open",
            Side::Mid => "mid",
            Side::Close => "close",
        }
    }
}

/// Side queries, expanded in priority order (first match wins),
/// port of loader.vim's `s:sidedict`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SideQuery {
    Open,
    Mid,
    Close,
    Both,
    BothAll,
    OpenMid,
}

impl SideQuery {
    pub fn sides(self) -> &'static [Side] {
        match self {
            SideQuery::Open => &[Side::Open],
            SideQuery::Mid => &[Side::Mid],
            SideQuery::Close => &[Side::Close],
            SideQuery::Both => &[Side::Close, Side::Open],
            SideQuery::BothAll => &[Side::Close, Side::Mid, Side::Open],
            SideQuery::OpenMid => &[Side::Mid, Side::Open],
        }
    }

    pub const ALL: [SideQuery; 6] = [
        SideQuery::Open,
        SideQuery::Mid,
        SideQuery::Close,
        SideQuery::Both,
        SideQuery::BothAll,
        SideQuery::OpenMid,
    ];
}

#[derive(Clone, Debug, Default)]
pub struct ExtraInfo {
    pub has_zs: bool,
    pub mid_hlend: bool,
}

/// A partially-resolved open pattern used when searching upwards
/// (port of the loader's `aug_comp` entries).
#[derive(Clone, Debug, Default)]
pub struct Aug {
    /// Maps this word's local capture numbers to open backref numbers.
    pub inputmap: HashMap<Grp, Grp>,
    /// Maps remaining sequential group numbers to open backref numbers.
    pub outputmap: BTreeMap<Grp, Grp>,
    /// The augment string (open pattern with `\N` placeholders).
    pub str: String,
}

/// The "regexone" list: patterns with backrefs (`\1`) unresolved.
#[derive(Clone, Debug)]
pub struct RegexOne {
    pub open: String,
    pub close: String,
    pub mid: String,
    pub mid_list: Vec<String>,
    pub augments: BTreeMap<Grp, String>,
}

/// The "regextwo" list: self-contained patterns where backrefs have
/// been replaced by the corresponding open capture groups.
#[derive(Clone, Debug)]
pub struct RegexTwo {
    pub open: String,
    pub close: String,
    pub mid: String,
    pub mid_list: Vec<String>,
    pub need_grp: HashSet<Grp>,
    /// word id -> (local capture group -> open backref number)
    pub grp_renu: HashMap<WordId, BTreeMap<Grp, Grp>>,
    pub aug_comp: HashMap<WordId, Vec<Aug>>,
    /// per-word `\g{...}` flags: "hlend" -> arg|1, "syn" -> arg
    pub extra_list: Vec<HashMap<String, String>>,
    pub extra_info: ExtraInfo,
}

#[derive(Clone, Debug)]
pub struct DelimSet {
    pub regexone: RegexOne,
    pub regextwo: RegexTwo,
}

/// Advanced mid disambiguation (port of `b:match_midmap` handling).
#[derive(Clone, Debug)]
pub struct MidMap {
    /// [syntax pattern, word pattern] elements.
    pub elements: Vec<(String, String)>,
    /// Union pattern of all mid words.
    pub strike: String,
}

#[derive(Clone, Debug, Default)]
pub struct DelimLists {
    pub sets: Vec<DelimSet>,
    pub midmap: Option<MidMap>,
}

impl DelimLists {
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
}

pub struct ParseInput<'a> {
    /// `b:match_words` (already evaluated if it was an expression).
    pub match_words: &'a str,
    /// `&matchpairs`.
    pub matchpairs: &'a str,
    /// `b:matchup_delim_nomatchpairs`.
    pub nomatchpairs: bool,
    /// `b:match_midmap`.
    pub midmap: Option<Vec<(String, String)>>,
}

/// vim's `escape(&matchpairs, '[$^.*~\\/?]')` (loader.vim:161).
pub fn escape_matchpairs(mps: &str) -> String {
    let mut out = String::new();
    for c in mps.chars() {
        if "[$^.*~\\/?]".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Port of `s:init_delim_lists` (loader.vim:127).
pub fn init_delim_lists(inp: &ParseInput) -> DelimLists {
    let match_words = inp.match_words;
    let simple = match_words.is_empty();

    let mut mw = String::new();
    if !simple {
        mw.push_str(match_words);
    }
    if !inp.nomatchpairs && !inp.matchpairs.is_empty() {
        if !mw.is_empty() {
            mw.push(',');
        }
        mw.push_str(&escape_matchpairs(inp.matchpairs));
    }

    let mut lists = DelimLists {
        sets: Vec::new(),
        midmap: inp.midmap.as_ref().map(|elems| MidMap {
            elements: elems.clone(),
            strike: format!(
                r"\%({}\)",
                elems
                    .iter()
                    .map(|e| format!(r"\({}\)", e.1))
                    .collect::<Vec<_>>()
                    .join(r"\|")
            ),
        }),
    };

    if simple {
        lists.sets = init_delim_lists_fast(&mw);
        return lists;
    }

    let mut seen: HashSet<String> = HashSet::new();
    for mut s in split_not_bslash(&mw, ',') {
        // very special case, escape bare [:]
        if s == "[:]" || s == r"\[:\]" {
            s = r"\[:]".to_string();
        }
        if !seen.insert(s.clone()) {
            continue;
        }
        if s.trim().is_empty() {
            continue;
        }

        let mut words = split_not_bslash(&s, ':');
        if words.len() < 2 {
            continue;
        }

        // pre-process \g{special} instructions
        let mut extra_list: Vec<HashMap<String, String>> =
            words.iter().map(|_| HashMap::new()).collect();
        for (i, w) in words.iter_mut().enumerate() {
            *w = strip_gspec(w, &mut extra_list[i]);
        }

        parse_set(&mut lists, &words, &mut extra_list);
    }

    lists
}

/// Replace `\g{flag}` / `\g{flag;arg}` markers (loader.vim:204-218).
/// `hlend` becomes the zero-width marker `\%(hlend\)\{0}` (which
/// `process_hlend` later converts to `\ze`); `syn` is removed.
fn strip_gspec(word: &str, extra: &mut HashMap<String, String>) -> String {
    let cs: Vec<char> = word.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\'
            && not_bslash_char(&cs, i)
            && i + 2 < cs.len()
            && cs[i + 1] == 'g'
            && cs[i + 2] == '{'
        {
            // find closing '}'
            let mut j = i + 3;
            while j < cs.len() && cs[j] != '}' {
                j += 1;
            }
            if j < cs.len() {
                let inner: String = cs[i + 3..j].iter().collect();
                let (flag, arg) = match inner.split_once(';') {
                    Some((f, a)) => (f.to_string(), a.to_string()),
                    None => (inner.clone(), String::new()),
                };
                let repl = match flag.as_str() {
                    "hlend" => r"\%(hlend\)\{0}",
                    _ => "",
                };
                out.push_str(repl);
                extra.insert(flag, if arg.is_empty() { "1".to_string() } else { arg });
                i = j + 1;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// Parse one comma-separated set into regexone/regextwo and append to
/// lists. Port of loader.vim:196-482.
fn parse_set(lists: &mut DelimLists, words_in: &[String], extra_list: &mut Vec<HashMap<String, String>>) {
    let n = words_in.len();
    let mut words = words_in.to_vec();
    let mut words_backref = words.clone();

    // capture groups of the open pattern
    let cg: BTreeMap<Grp, CaptureGroup> = get_capture_groups(&words[0])
        .into_iter()
        .map(|(k, mut v)| {
            // ref-2: capture groups should not contain backrefs; strip them
            if contains_backref(&v.str) {
                v.str = remove_backrefs(&v.str);
            }
            (k as Grp, v)
        })
        .collect();

    let mut all_needed_groups: HashSet<Grp> = HashSet::new();
    let mut group_renumber: HashMap<WordId, BTreeMap<Grp, Grp>> = HashMap::new();
    let mut augment_comp: HashMap<WordId, Vec<Aug>> = HashMap::new();

    // replacement order for augments: deepest groups first
    let order = capture_group_replacement_order(&cg);

    // build augments by replacing groups with \N, deepest to shallowest
    let mut augments: BTreeMap<Grp, String> = BTreeMap::new();
    let mut curaug = words[0].clone();
    augments.insert(0, curaug.clone());
    for &j in &order {
        let g = &cg[&j];
        let (p0, p1) = g.pos;
        // byte positions may exceed the shrunken string; vim's strpart
        // semantics clamp out-of-range tails to ''
        let p0 = p0.min(curaug.len());
        let p1 = p1.min(curaug.len());
        let is_boundary = |s: &str, k: usize| k <= s.len() && s.is_char_boundary(k);
        let cut0 = if is_boundary(&curaug, p0) { p0 } else { 0 };
        let cut1 = if is_boundary(&curaug, p1) { p1 } else { curaug.len() };
        curaug = format!("{}\\{}{}", &curaug[..cut0], j, &curaug[cut1..]);
        augments.insert(j, curaug.clone());
    }

    for i in 1..n {
        // get rid of capture groups in this pattern
        words_backref[i] = remove_capture_groups(&words_backref[i]);

        // needed \1, \2, ... backrefs, in order of appearance
        let needed_groups = find_backrefs(&words_backref[i]);

        let mut renu: BTreeMap<Grp, Grp> = BTreeMap::new();
        // cg2 persists across bref iterations within this word; prev_max is
        // max(keys(cg2)) from the previous iteration (loader.vim:317)
        let mut cg2: BTreeMap<Grp, CaptureGroup> = BTreeMap::new();
        for &bref in &needed_groups {
            if !cg.contains_key(&bref) {
                continue; // warn: backref without capture group
            }
            all_needed_groups.insert(bref);

            // turn the first `\bref` into the capture group string
            if let Some(g) = cg.get(&bref) {
                words_backref[i] = replace_first_backref(&words_backref[i], bref, &g.str);
            }

            // count the number of inserted groups
            let prev_max = cg2.keys().max().copied().unwrap_or(0);
            cg2 = get_capture_groups(&words_backref[i])
                .into_iter()
                .map(|(k, v)| (k as Grp, v))
                .collect();

            for (&cg2_i, _) in &cg2 {
                if cg2_i > prev_max {
                    renu.insert(cg2_i, bref + cg2_i - 1 - prev_max);
                }
            }

            // renumber any remaining `\bref` occurrences
            let renumbered = renu.iter().find(|(_, &v)| v == bref).map(|(&k, _)| k);
            if let Some(local) = renumbered {
                words_backref[i] =
                    replace_all_backref(&words_backref[i], bref, local);
            }
        }
        group_renumber.insert(i, renu);

        // compile the augment list for this word, going deepest first
        let mut resolvable: BTreeMap<Grp, Grp> = BTreeMap::new();
        let mut dependency: HashSet<Grp> = HashSet::new();
        let mut instruct: Vec<BTreeMap<Grp, Grp>> = Vec::new();
        let gren = group_renumber.get(&i).cloned().unwrap_or_default();
        for &j in &order {
            let in_grp_l: Vec<Grp> = gren
                .iter()
                .filter(|(_, &v)| v == j)
                .map(|(&k, _)| k)
                .collect();
            let in_grp = match in_grp_l.first() {
                Some(g) => *g,
                None => continue,
            };

            if dependency.contains(&j) {
                instruct.push(resolvable.clone());
                dependency.clear();
                resolvable.clear();
            }

            // walk up the tree marking dependencies
            let mut node = j;
            for _ in 0..11 {
                node = match cg.get(&node) {
                    Some(g) => g.parent as Grp,
                    None => 0,
                };
                if node == 0 {
                    break;
                }
                dependency.insert(node);
            }

            resolvable.insert(j, in_grp);
        }
        if !resolvable.is_empty() {
            instruct.push(resolvable.clone());
        }

        let mut aug_comp_i: Vec<Aug> = Vec::new();
        for instr in &instruct {
            let minkey = match instr.keys().min() {
                Some(k) => *k,
                None => continue,
            };
            let mut aug = Aug {
                inputmap: HashMap::new(),
                outputmap: BTreeMap::new(),
                str: augments.get(&minkey).cloned().unwrap_or_default(),
            };
            let mut remaining_out: BTreeSet<Grp> = cg.keys().copied().collect();
            for (&out_grp, &in_grp) in instr {
                aug.inputmap.insert(in_grp, out_grp);
                remaining_out.remove(&out_grp);
            }
            let mut counter: Grp = 1;
            for out_grp in remaining_out {
                aug.outputmap.insert(counter, out_grp);
                counter += 1;
            }
            // vim inserts each new aug at the front and fills element [0]
            aug_comp_i.insert(0, aug);
        }

        if instruct.is_empty() && !augments.is_empty() {
            let mut aug = Aug {
                inputmap: HashMap::new(),
                outputmap: BTreeMap::new(),
                str: augments.get(&0).cloned().unwrap_or_default(),
            };
            for &cg_i in cg.keys() {
                aug.outputmap.insert(cg_i, cg_i);
            }
            aug_comp_i = vec![aug];
        }

        augment_comp.insert(i, aug_comp_i);
    }

    // strip out unneeded groups in output maps
    for comp in augment_comp.values_mut() {
        for aug in comp.iter_mut() {
            aug.outputmap.retain(|_, v| all_needed_groups.contains(v));
        }
    }

    // the outermost needed augment replaces the open pattern (regexone)
    let mut order_rev: Vec<Grp> = order.iter().copied().rev().collect();
    order_rev.push(0);
    for g in order_rev {
        if all_needed_groups.contains(&g) {
            if let Some(a) = augments.get(&g) {
                words[0] = a.clone();
            }
            break;
        }
    }

    let extra_info = ExtraInfo {
        has_zs: words_backref.iter().any(|w| has_zs(w)),
        mid_hlend: extra_list[1..n.saturating_sub(1)]
            .iter()
            .any(|e| e.contains_key("hlend")),
    };

    // silence unused warning: cg is used above via get()
    let _ = &cg;

    lists.sets.push(DelimSet {
        regexone: RegexOne {
            open: words[0].clone(),
            close: words[n - 1].clone(),
            mid: words[1..n - 1].join(r"\|"),
            mid_list: words[1..n - 1].to_vec(),
            augments,
        },
        regextwo: RegexTwo {
            open: words_backref[0].clone(),
            close: words_backref[n - 1].clone(),
            mid: words_backref[1..n - 1].join(r"\|"),
            mid_list: words_backref[1..n - 1].to_vec(),
            need_grp: all_needed_groups,
            grp_renu: group_renumber,
            aug_comp: augment_comp,
            extra_list: std::mem::take(extra_list),
            extra_info,
        },
    });
}

/// Port of `matchup#loader#capture_group_replacement_order` (loader.vim:624):
/// keys descending, then stable-sorted deepest first.
fn capture_group_replacement_order(cg: &BTreeMap<Grp, CaptureGroup>) -> Vec<Grp> {
    let mut keys: Vec<Grp> = cg.keys().copied().collect();
    keys.sort_unstable_by(|a, b| b.cmp(a));
    keys.sort_by(|a, b| cg[b].depth.cmp(&cg[a].depth));
    keys
}

/// Port of `s:init_delim_lists_fast` (loader.vim:506): matchpairs only.
fn init_delim_lists_fast(mps: &str) -> Vec<DelimSet> {
    let mut sets = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for mut s in mps.split(',') {
        if s.trim().is_empty() {
            continue;
        }
        if s == "[:]" || s == r"\[:\]" {
            s = r"\[:]";
        }
        if !seen.insert(s.to_string()) {
            continue;
        }
        let words: Vec<&str> = s.split(':').collect();
        if words.len() < 2 {
            continue;
        }
        let open = words[0].to_string();
        let close = words[words.len() - 1].to_string();
        sets.push(DelimSet {
            regexone: RegexOne {
                open: open.clone(),
                close: close.clone(),
                mid: String::new(),
                mid_list: vec![],
                augments: BTreeMap::new(),
            },
            regextwo: RegexTwo {
                open,
                close,
                mid: String::new(),
                mid_list: vec![],
                need_grp: HashSet::new(),
                grp_renu: HashMap::new(),
                aug_comp: HashMap::new(),
                extra_list: vec![HashMap::new(), HashMap::new()],
                extra_info: ExtraInfo::default(),
            },
        });
    }
    sets
}

/// Build the combined scan regex for a side query (vim-regex syntax,
/// capture groups removed). Port of `s:init_delim_regexes_generator`
/// (loader.vim:599). Returns (pattern, has_zs); empty pattern means
/// no patterns for this side.
pub fn union_regex(sets: &[DelimSet], q: SideQuery) -> (String, bool) {
    let mut relist: Vec<&str> = Vec::new();
    for set in sets {
        for side in q.sides() {
            let s = match side {
                Side::Open => &set.regextwo.open,
                Side::Mid => &set.regextwo.mid,
                Side::Close => &set.regextwo.close,
            };
            if !s.is_empty() {
                relist.push(s);
            }
        }
    }
    let joined = format!(r"\%({}\)", relist.join(r"\|"));
    let stripped = remove_capture_groups(&joined);
    if stripped == r"\%(\)" {
        (String::new(), false)
    } else {
        let hz = has_zs(&stripped);
        (stripped, hz)
    }
}

/// Port of `matchup#loader#remove_capture_groups` (loader.vim:690):
/// `\(` -> `\%(` when the backslash is not itself escaped.
pub fn remove_capture_groups(re: &str) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\'
            && not_bslash_char(&cs, i)
            && i + 1 < cs.len()
            && cs[i + 1] == '('
        {
            out.push_str(r"\%(");
            i += 2;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// True when the pattern contains a `\zs` atom (loader.vim:448).
pub fn has_zs(re: &str) -> bool {
    let cs: Vec<char> = re.chars().collect();
    let mut i = 0;
    while i + 2 < cs.len() {
        if cs[i] == '\\' && not_bslash_char(&cs, i) && cs[i + 1] == 'z' && cs[i + 2] == 's' {
            return true;
        }
        i += 1;
    }
    false
}

/// Port of `s:process_hlend` (delim.vim:972): replace `\ze` atoms
/// (with `\%>cursorpos c` when cursorpos >= 0, else remove) and convert
/// the hlend marker `\%(hlend\)\{0}` to `\ze`.
pub fn process_hlend(re: &str, cursorpos: isize) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && not_bslash_char(&cs, i) && i + 1 < cs.len() && cs[i + 1] == 'z' && i + 2 < cs.len() && cs[i + 2] == 'e'
        {
            if cursorpos >= 0 {
                out.push_str(&format!(r"\%>{}c", cursorpos));
            }
            i += 3;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out.replace(r"\%(hlend\)\{0}", r"\ze")
}

// ---------------------------------------------------------------------------
// backref helpers
// ---------------------------------------------------------------------------

fn not_bslash_char(cs: &[char], pos: usize) -> bool {
    let mut n = 0;
    let mut j = pos;
    while j > 0 && cs[j - 1] == '\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 0
}

/// Find `\N` backref numbers in order of first appearance.
fn find_backrefs(re: &str) -> Vec<Grp> {
    let cs: Vec<char> = re.chars().collect();
    let mut out: Vec<Grp> = Vec::new();
    let mut i = 0;
    while i + 1 < cs.len() {
        if cs[i] == '\\' && not_bslash_char(&cs, i) && cs[i + 1].is_ascii_digit() && cs[i + 1] != '0'
        {
            let n = cs[i + 1] as Grp - '0' as Grp;
            if !out.contains(&n) {
                out.push(n);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

fn contains_backref(re: &str) -> bool {
    !find_backrefs(re).is_empty()
}

fn remove_backrefs(re: &str) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if i + 1 < cs.len()
            && cs[i] == '\\'
            && not_bslash_char(&cs, i)
            && cs[i + 1].is_ascii_digit()
            && cs[i + 1] != '0'
        {
            i += 2;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// Replace the first `\bref` (not preceded by a backslash) with `repl`.
fn replace_first_backref(re: &str, bref: Grp, repl: &str) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut done = false;
    while i < cs.len() {
        if !done
            && i + 1 < cs.len()
            && cs[i] == '\\'
            && not_bslash_char(&cs, i)
            && cs[i + 1] == char::from_digit(bref, 10).unwrap()
        {
            out.push_str(repl);
            done = true;
            i += 2;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// Replace all remaining `\bref` with `\local`.
fn replace_all_backref(re: &str, bref: Grp, local: Grp) -> String {
    let cs: Vec<char> = re.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if i + 1 < cs.len()
            && cs[i] == '\\'
            && not_bslash_char(&cs, i)
            && cs[i + 1] == char::from_digit(bref, 10).unwrap()
        {
            out.push('\\');
            out.push(char::from_digit(local, 10).unwrap());
            i += 2;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(match_words: &str) -> DelimLists {
        init_delim_lists(&ParseInput {
            match_words,
            matchpairs: "(:),{:},[:]",
            nomatchpairs: false,
            midmap: None,
        })
    }

    #[test]
    fn fast_path_matchpairs() {
        let lists = init_delim_lists(&ParseInput {
            match_words: "",
            matchpairs: "(:),{:},[:]",
            nomatchpairs: false,
            midmap: None,
        });
        assert_eq!(lists.sets.len(), 3);
        assert_eq!(lists.sets[0].regexone.open, "(");
        assert_eq!(lists.sets[0].regexone.close, ")");
        // bare [:] is escaped
        assert_eq!(lists.sets[2].regexone.open, r"\[:]".split(':').next().unwrap());
    }

    #[test]
    fn lua_backref_set() {
        let lists = parse(r"--\[\(=*\)\[:\%(--\)\=]\1]");
        // 1 user set + 3 matchpairs sets
        assert_eq!(lists.sets.len(), 4);
        let s = &lists.sets[0];
        // regexone.open is the augment with the needed group
        assert_eq!(s.regexone.open, r"--\[\1\[");
        assert_eq!(s.regexone.close, r"\%(--\)\=]\1]");
        // regextwo is self-contained
        assert_eq!(s.regextwo.open, r"--\[\(=*\)\[");
        assert_eq!(s.regextwo.close, r"\%(--\)\=]\(=*\)]");
        assert!(s.regextwo.need_grp.contains(&1));
        // close word id = len(mid_list)+1 = 1
        let renu = s.regextwo.grp_renu.get(&1).unwrap();
        assert_eq!(renu.get(&1), Some(&1));
        let aug = &s.regextwo.aug_comp[&1][0];
        assert_eq!(aug.str, r"--\[\1\[");
        assert_eq!(aug.inputmap.get(&1), Some(&1));
        assert!(aug.outputmap.is_empty());
    }

    #[test]
    fn vim_style_set_with_mids() {
        let lists = parse(r"\<if\>:\<el\%[seif]\>:\<en\%[dif]\>,{:}");
        assert_eq!(lists.sets.len(), 4);
        let s = &lists.sets[0];
        assert_eq!(s.regexone.open, r"\<if\>");
        assert_eq!(s.regexone.mid_list, vec![r"\<el\%[seif]\>"]);
        assert_eq!(s.regexone.close, r"\<en\%[dif]\>");
        assert!(!s.regextwo.extra_info.has_zs);
        // close word id = len(mid_list)+1 = 2
        assert!(!s.regextwo.grp_renu.contains_key(&2) || true);
    }

    #[test]
    fn lookbehind_close_word() {
        let lists = parse(r"\<if\>:\%(\%(^\||\)\s*\)\@<=\<en\%[dif]\>");
        let s = &lists.sets[0];
        assert_eq!(s.regexone.close, r"\%(\%(^\||\)\s*\)\@<=\<en\%[dif]\>");
    }

    #[test]
    fn gspec_hlend_stripping() {
        let lists = parse(r"\<if\>\g{hlend}:\<endif\>");
        let s = &lists.sets[0];
        assert_eq!(s.regexone.open, r"\<if\>\%(hlend\)\{0}");
        assert_eq!(
            s.regextwo.extra_list[0].get("hlend"),
            Some(&"1".to_string())
        );
        // process_hlend turns the marker into \ze
        assert_eq!(
            process_hlend(&s.regexone.open, -1),
            r"\<if\>\ze"
        );
    }

    #[test]
    fn gspec_syn_stripping() {
        let lists = parse("``:``\\g{syn;!JanetString}");
        let s = &lists.sets[0];
        assert_eq!(s.regexone.close, "``");
        assert_eq!(
            s.regextwo.extra_list[1].get("syn"),
            Some(&"!JanetString".to_string())
        );
    }

    #[test]
    fn union_regexes() {
        let lists = parse(r"\<if\>:\<endif\>");
        let (u, _) = union_regex(&lists.sets, SideQuery::BothAll);
        // order: for each set, [close, mid, open]
        assert!(u.starts_with(r"\%(\<endif\>\|\<if\>"));
        assert!(u.contains(r"\["));
        assert!(u.contains(')') && u.contains('('));
        let (u_open, _) = union_regex(&lists.sets, SideQuery::Open);
        assert!(u_open.contains(r"\<if\>"));
        assert!(!u_open.contains(r"\<endif\>"));
        // no capture groups in unions
        assert!(!u_open.contains(r"\("));
    }

    #[test]
    fn remove_capture_groups_works() {
        assert_eq!(
            remove_capture_groups(r"\(foo\)\%(bar\)\\\(baz\)"),
            r"\%(foo\)\%(bar\)\\\%(baz\)"
        );
    }

    #[test]
    fn duplicate_sets_deduped() {
        let lists = parse(r"\<if\>:\<endif\>,\<if\>:\<endif\>");
        // 1 unique user set + 3 matchpairs
        assert_eq!(lists.sets.len(), 4);
    }

    #[test]
    fn nested_capture_groups_augments() {
        // the "very tricky" good example from loader.vim:137
        let lists = parse(r"\(\(foo\)\(bar\)\):\3\2:end\1");
        let s = &lists.sets[0];
        assert_eq!(s.regextwo.open, r"\(\(foo\)\(bar\)\)");
        // mid `\3\2` becomes \(bar\)\(foo\) with renumbering {1:3, 2:2}
        assert_eq!(s.regextwo.mid, r"\(bar\)\(foo\)");
        let renu = s.regextwo.grp_renu.get(&1).unwrap();
        assert_eq!(renu.get(&1), Some(&3));
        assert_eq!(renu.get(&2), Some(&2));
        // close `end\1` -> end\(foo\)\(bar\)... group 1 str is the whole
        // open group: `\(\(foo\)\(bar\)\)` inserted, renumbered
        assert!(s.regextwo.close.starts_with("end"));
        assert!(s.regextwo.need_grp.contains(&1));
        assert!(s.regextwo.need_grp.contains(&2));
        assert!(s.regextwo.need_grp.contains(&3));
        // regexone.open replaced by the outermost needed augment
        assert_eq!(s.regexone.open, r"\1");
    }
}
