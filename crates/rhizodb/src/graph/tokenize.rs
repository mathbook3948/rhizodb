use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

const VERSION: u32 = 6;

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
    "a\u{e31} ก\u{e31}a\u{e31} \u{e31}ก ㅋㅋㅋ ㅠㅠ ㄱ가나 ﾡﾡﾥ \u{1100}\u{1100}\u{1161} \u{1113}\u{1161} \u{a960}\u{d7b0}",
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

/// What the terms are for. A document also keeps the single grapheme clusters of a paired-script
/// run, so a one-cluster query finds it, while a query of a longer run looks only for its pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Document,
    Query,
}

/// A term and where it occurs in its text. A word takes one position. A paired-script run takes
/// one position per grapheme cluster, and each pair and each single cluster sits at the position
/// of its first cluster. Neighbouring query terms are therefore at most two positions apart.
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
    let text: String = text.nfkc().collect();
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
/// grapheme clusters, because those scripts attach particles to words or leave out spaces
/// between words. A grapheme cluster goes wholly to a run or wholly to a word, by its first
/// character, so a run starts at a letter that is not a digit and never at a mark. In Hangul
/// the unit is each syllable and each jamo left over after normalization, since a grapheme
/// cluster of loose jamo such as "ㅋㅋㅋ" holds several letters a reader types and searches
/// one at a time. The same text always gives the same terms, in the same order, with
/// positions counted from zero.
pub(crate) fn terms(text: &str, purpose: Purpose) -> Vec<Term> {
    scan(&text.nfkc().collect::<String>(), purpose).terms
}

fn scan(text: &str, purpose: Purpose) -> Scan {
    let mut scan = Scan {
        purpose,
        terms: Vec::new(),
        starred: Vec::new(),
        run: String::new(),
        next: 0,
    };
    let mut segments = text.split_word_bounds().peekable();
    while let Some(segment) = segments.next() {
        if !segment.chars().any(char::is_alphanumeric) {
            scan.flush_run();
            continue;
        }
        let mut word = String::new();
        for cluster in segment.graphemes(true) {
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
        let pushed = word.chars().any(char::is_alphanumeric);
        if pushed {
            self.terms.push(Term {
                text: word.to_lowercase().nfkc().collect(),
                position: self.next,
            });
            self.next = self.at(1);
        }
        word.clear();
        pushed
    }

    fn flush_run(&mut self) {
        let run = std::mem::take(&mut self.run);
        let units: Vec<&str> = run
            .graphemes(true)
            .flat_map(|cluster| {
                if cluster.chars().any(is_hangul) {
                    cluster.split_inclusive(|_| true).collect()
                } else {
                    vec![cluster]
                }
            })
            .collect();
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

fn starts_run(character: char) -> bool {
    is_paired_script(character) && !character.is_numeric() && character.is_alphabetic()
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
