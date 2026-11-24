use unicode_normalization::IsNormalized;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::is_nfkc_quick;
use unicode_segmentation::UnicodeSegmentation;

const VERSION: u32 = 8;

const PROBE: &str = concat!(
    "Don't stop at 3.14, e-mail_me or ŌΔΟΣ İstanbul STRAßE Ǆemal ᲛᲐᲠᲗ ᏣᎳᎩ Ꭰ. ",
    "हिन्दी ภาษาไทย עִבְרִית العَرَبِيَّة 👩\u{200d}💻 a\u{0}\u{903}b x\u{7}\u{301}y \u{feff}z ",
    "서울에서 만나요 \u{1100}\u{1161}\u{11a8} 東京都に行く カタカナ ｶﾀｶﾅ 北京\u{2a6df}\u{31350}\u{323af} ",
    "\u{1e290}\u{1e291} \u{11f04}\u{11f05} \u{16d43}\u{16d44} \u{10d50}\u{10d51} \u{1e5d0}\u{1e5d1} ",
    "\u{2ebf0}\u{2ee5d} \u{1e030}\u{1e031} \u{0870}\u{0897} \u{1d7ff}\u{1df25} \u{10efc}\u{11b00} ",
    "\u{0378}\u{0379} \u{05ff}\u{08b5} \u{1e7e0}\u{1e7e7} \u{2b73a}\u{31350} \u{3134b}\u{e0001} ",
    "Cafe\u{301} A\u{30a}\u{323} ﬁ Ｘ² ① ㎏ \u{1109}\u{1165} \u{0928}\u{093c} \u{2126} \u{fb2c} ",
    "\u{1d15f} \u{2adc} \u{11099}\u{110ba} \u{1133b}\u{1133e} \u{0f73} \u{1e9b}\u{323} ",
    "ก่อน ๒๕๖๗ a\u{e48} ພາສາລາວ ភាសាខ្មែរ မြန်မာဘာသာ \u{1a20}\u{1a60}\u{1a21} \u{aa80}\u{aab0} ",
    "\u{19d1}\u{1950}\u{1951} \u{11700}\u{1171d}\u{116d0} \u{a9e0}\u{aa60} ",
    "a\u{e31} ก\u{e31}a\u{e31} \u{e31}ก ㅋㅋㅋ ㅠㅠ ㄱ가나 ﾡﾡﾥ \u{1100}\u{1100}\u{1161} \u{1113}\u{1161} \u{a960}\u{d7b0} ",
    "가\u{301} 한\u{200d}국 x \u{e31}ก \u{301}abc",
);

/// The tokenizer's version and its terms and their positions for a fixed probe text. The probe
/// exercises the Unicode tables the tokenizer depends on, including recently assigned and still
/// unassigned code points, so a change to those tables that touches the probe changes the
/// fingerprint even when the version does not.
pub(crate) fn fingerprint() -> Vec<u8> {
    let mut out = VERSION.to_be_bytes().to_vec();
    for purpose in [Purpose::Document, Purpose::Query] {
        for term in terms(PROBE, purpose) {
            out.extend_from_slice(term.text.as_bytes());
            out.push(0);
            out.extend_from_slice(&term.position.to_be_bytes());
        }
        out.push(0);
    }
    out
}

/// What the terms are for. A document also keeps the single units of a paired-script run, so a
/// one-unit query finds it, while a query of a longer run looks only for its pairs of units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Document,
    Query,
}

/// A term and where it occurs in its text. A word takes one position. A paired-script run takes
/// one position per unit, and each pair and each single unit sits at the position of its first
/// unit. A unit is a grapheme cluster, except in Hangul, where it is a syllable or a jamo left
/// over after normalization, with the marks and joiners of its cluster that follow it.
/// Neighbouring query terms are therefore at most two positions apart.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Term {
    pub(crate) text: String,
    pub(crate) position: u32,
}

/// One part of a keyword query.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Clause {
    /// Matches a node that holds the term.
    Term(String),
    /// Matches a node that holds a term that starts with this text.
    Prefix(String),
    /// Matches a node that holds every term at the same distances from one another as in the
    /// query. The first term is at position zero.
    Phrase(Vec<Term>),
}

/// Parses a keyword query into its clauses, sorted and without repeats. Text between two
/// double quotes is a phrase, and an unclosed quote runs to the end of the text. A phrase of one
/// term is that term, and a phrase of none is nothing. Every term outside quotes is a clause of
/// its own, and a word directly followed by `*` outside quotes is a prefix. A `*` after a
/// paired-script run changes nothing, because such a run already matches inside longer runs.
pub(crate) fn clauses(text: &str) -> Vec<Clause> {
    let text = nfkc(text);
    let mut clauses = Vec::new();
    for (index, piece) in text.split('"').enumerate() {
        let scan = scan(piece, Purpose::Query);
        if index % 2 == 0 {
            for (position, term) in scan.terms.into_iter().enumerate() {
                if scan.starred.contains(&position) {
                    clauses.push(Clause::Prefix(term.text));
                } else {
                    clauses.push(Clause::Term(term.text));
                }
            }
        } else if let [term] = scan.terms.as_slice() {
            clauses.push(Clause::Term(term.text.clone()));
        } else if !scan.terms.is_empty() {
            clauses.push(Clause::Phrase(scan.terms));
        }
    }
    clauses.sort();
    clauses.dedup();
    clauses
}

/// Splits `text` into lowercase terms at Unicode word boundaries, dropping control characters,
/// so no term holds a zero byte. Text and terms are in Unicode normalization form NFKC, so
/// canonically equivalent text gives the same terms, and so do compatibility forms such as
/// ligatures and full-width letters. Runs of Hangul, Han, Hiragana, Katakana, and the scripts
/// that Unicode line breaking leaves to a dictionary, such as Thai, become overlapping pairs of
/// units, as [`Term`] defines them, because those scripts attach particles to words or leave
/// out spaces between words. A grapheme cluster goes wholly to a run or wholly to a word, by its first
/// character, so a run starts at a letter that is not a digit. A cluster that starts with
/// whitespace, or with a character the Unicode grapheme rules join to the character before it
/// (Grapheme_Cluster_Break Extend, SpacingMark, or ZWJ), has no base letter and separates words
/// like a space; a mark with Grapheme_Cluster_Break Other, such as a few spacing marks of
/// Brahmic scripts, is still taken for a letter. A word starts and ends at a grapheme cluster whose
/// first character is a letter or digit, so punctuation that a script change leaves at its edge,
/// and a mark that follows punctuation, are not part of it. Control characters are dropped. Hangul units
/// are smaller than grapheme clusters because a cluster of loose jamo such as "ㅋㅋㅋ" holds
/// several letters a reader types and searches one at a time. The same text always gives the
/// same terms, in the same order, with positions counted from zero.
pub(crate) fn terms(text: &str, purpose: Purpose) -> Vec<Term> {
    scan(&nfkc(text), purpose).terms
}

fn nfkc(text: &str) -> std::borrow::Cow<'_, str> {
    if is_nfkc_quick(text.chars()) == IsNormalized::Yes {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.nfkc().collect())
    }
}

fn scan(text: &str, purpose: Purpose) -> Scan {
    scan_by(text, purpose, str::is_ascii)
}

fn scan_by(text: &str, purpose: Purpose, ascii: impl Fn(&str) -> bool) -> Scan {
    let mut scan = Scan {
        purpose,
        terms: Vec::new(),
        starred: Vec::new(),
        run: String::new(),
        next: 0,
    };
    let mut segments = text.split_word_bounds().peekable();
    while let Some(segment) = segments.next() {
        if ascii(segment) {
            scan.flush_run();
            if scan.push_ascii_segment(segment)
                && segments.peek().is_some_and(|next| next.starts_with('*'))
            {
                scan.starred.push(scan.terms.len().saturating_sub(1));
            }
            continue;
        }
        if !segment.chars().any(char::is_alphanumeric) {
            scan.flush_run();
            continue;
        }
        let mut word = String::new();
        for cluster in segment.graphemes(true) {
            if has_no_base(cluster) {
                scan.push_word(&mut word);
                scan.flush_run();
                continue;
            }
            let kept = cluster.chars().filter(|character| !character.is_control());
            if cluster.chars().next().is_some_and(starts_run) {
                scan.push_word(&mut word);
                scan.run.extend(kept);
            } else {
                scan.flush_run();
                word.extend(kept);
            }
        }
        if scan.push_word(&mut word) && segments.peek().is_some_and(|next| next.starts_with('*')) {
            scan.starred.push(scan.terms.len().saturating_sub(1));
        }
    }
    scan.flush_run();
    scan
}

struct Scan {
    purpose: Purpose,
    terms: Vec<Term>,
    starred: Vec<usize>,
    run: String,
    next: u32,
}

impl Scan {
    fn at(&self, offset: usize) -> u32 {
        self.next
            .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX))
    }

    fn push_word(&mut self, word: &mut String) -> bool {
        let trimmed = between_alphanumeric_bases(word);
        let pushed = !trimmed.is_empty();
        if pushed {
            self.terms.push(Term {
                text: trimmed.to_lowercase().nfkc().collect(),
                position: self.next,
            });
            self.next = self.at(1);
        }
        word.clear();
        pushed
    }

    fn push_ascii_segment(&mut self, segment: &str) -> bool {
        let mut pushed = false;
        for word in segment.split(char::is_whitespace) {
            let trimmed = word.trim_matches(|character: char| !character.is_ascii_alphanumeric());
            pushed = !trimmed.is_empty();
            if pushed {
                let text = trimmed
                    .chars()
                    .filter(|character| !character.is_control())
                    .map(|character| character.to_ascii_lowercase())
                    .collect();
                self.terms.push(Term {
                    text,
                    position: self.next,
                });
                self.next = self.at(1);
            }
        }
        pushed
    }

    fn flush_run(&mut self) {
        let run = std::mem::take(&mut self.run);
        let units: Vec<&str> = run.graphemes(true).flat_map(split_before_hangul).collect();
        if let [single] = units.as_slice() {
            self.terms.push(Term {
                text: (*single).to_owned(),
                position: self.next,
            });
        } else {
            for (offset, pair) in units.windows(2).enumerate() {
                self.terms.push(Term {
                    text: pair.concat(),
                    position: self.at(offset),
                });
            }
            if self.purpose == Purpose::Document {
                for (offset, unit) in units.iter().enumerate() {
                    self.terms.push(Term {
                        text: (*unit).to_owned(),
                        position: self.at(offset),
                    });
                }
            }
        }
        self.next = self.at(units.len());
    }
}

fn between_alphanumeric_bases(word: &str) -> &str {
    let mut bounds: Option<(usize, usize)> = None;
    for (at, cluster) in word.grapheme_indices(true) {
        if cluster.chars().next().is_some_and(char::is_alphanumeric) {
            let start = bounds.map_or(at, |(start, _)| start);
            bounds = Some((start, at + cluster.len()));
        }
    }
    bounds
        .and_then(|(start, end)| word.get(start..end))
        .unwrap_or("")
}

fn has_no_base(cluster: &str) -> bool {
    let Some(first) = cluster.chars().next() else {
        return true;
    };
    if first.is_whitespace() {
        return true;
    }
    let mut buffer = [b'a'; 5];
    let length = first.encode_utf8(&mut buffer[1..]).len();
    buffer
        .get(..=length)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|text| text.graphemes(true).count() == 1)
}

fn starts_run(character: char) -> bool {
    is_paired_script(character) && !character.is_numeric() && character.is_alphabetic()
}

fn split_before_hangul(cluster: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut start = 0;
    for (at, character) in cluster.char_indices().skip(1) {
        if is_hangul(character) {
            units.extend(cluster.get(start..at));
            start = at;
        }
    }
    units.extend(cluster.get(start..));
    units
}

fn is_hangul(character: char) -> bool {
    matches!(
        u32::from(character),
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF | 0xFFA0..=0xFFDC
    )
}

fn is_paired_script(character: char) -> bool {
    matches!(
        u32::from(character),
        0x0E00..=0x0EFF
            | 0x1000..=0x109F
            | 0x1100..=0x11FF
            | 0x1780..=0x17FF
            | 0x1950..=0x19FF
            | 0x1A20..=0x1AAF
            | 0x2E80..=0x2FDF
            | 0x3040..=0x30FF
            | 0x3130..=0x318F
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xA9E0..=0xA9FF
            | 0xAA60..=0xAADF
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFFDC
            | 0x116D0..=0x116FF
            | 0x11700..=0x1174F
            | 0x20000..=0x323AF
    )
}

#[cfg(test)]
#[path = "tokenize.test.rs"]
mod tests;
