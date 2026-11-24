use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::engine::layout::ExtentId;

const TABLE: TableId = TableId(0x100);
const SEED: u64 = 7;

fn at() -> BlockRef {
    BlockRef::new(ExtentId(2), 9).unwrap()
}

fn blob(index: usize, length: usize) -> BlobRef {
    BlobRef {
        first: RunRef {
            at: BlockRef::new(ExtentId(3), index as u32).unwrap(),
            blocks: crate::engine::blob::run_blocks(
                length.min(crate::engine::blob::RUN_PAYLOAD_LIMIT),
            ),
            lsn: 4,
        },
        length: length as u64,
        checksum: index as u64,
    }
}

fn sealed(content: &Content, level: u8, long_keys: &mut Vec<Vec<u8>>) -> Vec<u8> {
    let (format, cells, body) = encode(content, |key| {
        long_keys.push(key.to_vec());
        Ok(blob(long_keys.len() - 1, key.len()))
    })
    .unwrap();
    let mut page = vec![0; PAGE_HEADER];
    page.extend_from_slice(&body);
    page.resize(BLOCK, 0);
    PageHeader {
        kind: if level == 0 {
            PageKind::Leaf
        } else {
            PageKind::Branch
        },
        format,
        table: TABLE,
        level,
        flags: 0,
        cells,
        free: 0,
        at: at(),
        lsn: 4,
        generation: 2,
    }
    .seal(SEED, &mut page)
    .unwrap();
    page
}

fn decoded(page: &[u8], long_keys: &[Vec<u8>], low: &Bytes) -> (Content, Vec<BlobRef>) {
    let header = PageHeader::open(SEED, page, at()).unwrap();
    let node = Node::parse(&header, page, TABLE).unwrap();
    let load = |found: &BlobRef| Ok(long_keys[found.first.at.block() as usize].clone());
    node.content(&load, low).unwrap()
}

fn leaf(entries: &[(&[u8], StoredValue)]) -> Content {
    Content::Leaf(
        entries
            .iter()
            .map(|(key, value)| LeafEntry {
                key: Bytes::from(*key),
                value: value.clone(),
            })
            .collect(),
    )
}

fn inline(bytes: &[u8]) -> StoredValue {
    StoredValue::Inline(Bytes::from(bytes))
}

#[test]
fn a_leaf_of_equal_widths_uses_the_fixed_format() {
    let content = leaf(&[
        (b"key-01", inline(b"aa")),
        (b"key-02", inline(b"bb")),
        (b"key-03", inline(b"cc")),
    ]);
    let page = sealed(&content, 0, &mut Vec::new());
    assert_eq!(page[1], FIXED);
    assert_eq!(decoded(&page, &[], &Bytes::from(&[][..])).0, content);
}

#[test]
fn a_leaf_of_mixed_widths_uses_the_variable_format() {
    let content = leaf(&[
        (b"", inline(b"")),
        (b"a", inline(&[1; 1024])),
        (b"ab", StoredValue::External(blob(0, 5000))),
        (b"b", inline(b"x")),
    ]);
    let page = sealed(&content, 0, &mut Vec::new());
    assert_eq!(page[1], VARIABLE);
    assert_eq!(decoded(&page, &[], &Bytes::from(&[][..])).0, content);
}

#[test]
fn long_keys_keep_a_stub_and_compare_through_their_run() {
    let mut long_keys = Vec::new();
    let first = [vec![b'x'; 1500], vec![1]].concat();
    let second = [vec![b'x'; 1500], vec![2]].concat();
    let content = leaf(&[(&first, inline(b"1")), (&second, inline(b"2"))]);
    let page = sealed(&content, 0, &mut long_keys);
    assert_eq!(long_keys, [first.clone(), second.clone()]);
    let (found, owned) = decoded(&page, &long_keys, &Bytes::from(&[][..]));
    assert_eq!(found, content);
    assert_eq!(owned.len(), 2);
    let header = PageHeader::open(SEED, &page, at()).unwrap();
    let node = Node::parse(&header, &page, TABLE).unwrap();
    let load = |found: &BlobRef| Ok(long_keys[found.first.at.block() as usize].clone());
    assert_eq!(node.search(&second, &load).unwrap(), 1);
    assert_eq!(node.search(&first, &load).unwrap(), 0);
    assert_eq!(
        node.search(&[vec![b'x'; 1500], vec![1, 0]].concat(), &load)
            .unwrap(),
        1
    );
    assert_eq!(node.search(b"y", &load).unwrap(), 2);
    assert_eq!(node.search(b"x", &load).unwrap(), 0);
    let refuse = |_: &BlobRef| Err(EngineError::corrupt("loaded"));
    assert_eq!(node.search(b"a", &refuse).unwrap(), 0);
}

#[test]
fn a_branch_searches_by_its_low_keys() {
    let entries: Vec<BranchEntry> = [&b""[..], b"d", b"m", b"t"]
        .iter()
        .enumerate()
        .map(|(index, low)| BranchEntry {
            low: Bytes::from(*low),
            child: ChildRef {
                at: BlockRef::new(ExtentId(5), index as u32).unwrap(),
                lsn: 3,
            },
        })
        .collect();
    let content = Content::Branch(entries.clone());
    let page = sealed(&content, 1, &mut Vec::new());
    assert_eq!(decoded(&page, &[], &Bytes::from(&[][..])).0, content);
    let header = PageHeader::open(SEED, &page, at()).unwrap();
    let node = Node::parse(&header, &page, TABLE).unwrap();
    let load = |_: &BlobRef| Err(EngineError::corrupt("no long keys"));
    for (target, child) in [
        (&b"a"[..], 0),
        (b"d", 1),
        (b"e", 1),
        (b"m", 2),
        (b"s", 2),
        (b"t", 3),
        (b"z", 3),
    ] {
        assert_eq!(node.search(target, &load).unwrap(), child, "{target:?}");
    }
    assert!(node.value(0).is_err());
    assert!(node.key(0).is_err());
}

#[test]
fn the_page_size_estimate_matches_the_encoding() {
    let content = leaf(&[
        (b"prefix-a", inline(b"1")),
        (b"prefix-bb", inline(b"22")),
        (&[b'p'; 2000], inline(b"3")),
    ]);
    let Content::Leaf(entries) = &content else {
        unreachable!()
    };
    let mut size = PageSize::default();
    let mut bytes = 0;
    for entry in entries {
        (size, bytes) = size.with(Some(&entry.key), value_bytes(&entry.value));
    }
    let (_, _, body) = encode(&content, |key| Ok(blob(0, key.len()))).unwrap();
    assert_eq!(bytes, PAGE_HEADER + body.len());
}

fn entry_strategy() -> impl Strategy<Value = (Vec<u8>, StoredValue)> {
    (
        prop_oneof![vec(0u8..3, 0..6), vec(any::<u8>(), 1020..1100)],
        prop_oneof![
            vec(any::<u8>(), 0..40).prop_map(|bytes| StoredValue::Inline(Bytes::from(bytes))),
            (1025usize..100_000).prop_map(|length| StoredValue::External(blob(1, length))),
        ],
    )
}

proptest! {
    #[test]
    fn any_page_of_cells_round_trips(
        entries in proptest::collection::btree_map(any::<u8>(), entry_strategy(), 1..4),
    ) {
        let entries: Vec<(Vec<u8>, StoredValue)> = entries
            .into_iter()
            .map(|(first, (mut key, value))| {
                key.insert(0, first);
                (key, value)
            })
            .collect();
        let content = Content::Leaf(
            entries
                .iter()
                .map(|(key, value)| LeafEntry { key: Bytes::from(key.as_slice()), value: value.clone() })
                .collect(),
        );
        let mut long_keys = Vec::new();
        let page = sealed(&content, 0, &mut long_keys);
        prop_assert_eq!(decoded(&page, &long_keys, &Bytes::from(&[][..])).0, content);
    }

    #[test]
    fn a_damaged_but_resealed_page_never_panics(
        content_seed in any::<u8>(),
        damage in vec((0usize..BLOCK, any::<u8>()), 1..20),
        level in 0u8..2,
    ) {
        let content = if level == 0 {
            leaf(&[(b"a", inline(&[content_seed; 30])), (b"b", StoredValue::External(blob(0, 2000))), (&[b'q'; 1100], inline(b"x"))])
        } else {
            Content::Branch(vec![
                BranchEntry { low: Bytes::from(&b""[..]), child: ChildRef { at: at(), lsn: 1 } },
                BranchEntry { low: Bytes::from(&[b'w'; 1200][..]), child: ChildRef { at: at(), lsn: 1 } },
            ])
        };
        let mut long_keys = Vec::new();
        let mut page = sealed(&content, level, &mut long_keys);
        for (offset, byte) in damage {
            page[offset.max(PAGE_HEADER)] = byte;
        }
        let mut header = PageHeader::open(SEED, &page, at()).ok();
        if header.is_none() {
            let original = PageHeader::open(SEED, &sealed(&content, level, &mut Vec::new()), at()).unwrap();
            original.seal(SEED, &mut page).unwrap();
            header = PageHeader::open(SEED, &page, at()).ok();
        }
        let Some(header) = header else { return Ok(()); };
        let Ok(node) = Node::parse(&header, &page, TABLE) else { return Ok(()); };
        let load = |found: &BlobRef| -> Result<Vec<u8>, EngineError> {
            long_keys.get(found.first.at.block() as usize).cloned().ok_or_else(|| EngineError::corrupt("no run"))
        };
        for index in 0..node.cells() {
            let _ = node.key(index).and_then(|key| key.full(&load));
            let _ = node.value(index);
            let _ = node.child(index);
        }
        let _ = node.search(b"a", &load);
        let _ = node.search(&[b'w'; 1300], &load);
        let _ = node.content(&load, &Bytes::from(&[][..]));
    }
}

fn key_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => vec(0u8..4, 0..12),
        1 => vec(any::<u8>(), 0..24),
        1 => vec(0u8..2, 1020..1100),
    ]
}

fn long_key_loader(
    long_keys: &[Vec<u8>],
) -> impl Fn(&BlobRef) -> Result<Vec<u8>, EngineError> + '_ {
    |found: &BlobRef| {
        long_keys
            .get(found.first.at.block() as usize)
            .cloned()
            .ok_or_else(|| EngineError::corrupt("no run"))
    }
}

proptest! {
    #[test]
    fn compare_bytes_orders_as_slices_do(left in key_strategy(), right in key_strategy()) {
        prop_assert_eq!(compare_bytes(&left, &right), left.cmp(&right));
        prop_assert_eq!(compare_bytes(&left, &left), Ordering::Equal);
    }

    #[test]
    fn a_leaf_finds_the_first_key_at_or_above_the_target(
        keys in proptest::collection::btree_set(key_strategy(), 1..30),
        targets in vec(key_strategy(), 1..20),
        same_width in any::<bool>(),
    ) {
        let keys: Vec<Vec<u8>> = keys.into_iter().collect();
        let content = leaf(
            &keys
                .iter()
                .map(|key| {
                    let value = if same_width { vec![1; 8] } else { vec![2; key.len() % 5] };
                    (key.as_slice(), inline(&value))
                })
                .collect::<Vec<_>>(),
        );
        let mut long_keys = Vec::new();
        let page = sealed(&content, 0, &mut long_keys);
        let header = PageHeader::open(SEED, &page, at()).unwrap();
        let node = Node::parse(&header, &page, TABLE).unwrap();
        let load = long_key_loader(&long_keys);
        for target in targets.iter().chain(&keys) {
            let first_at_or_above = keys.partition_point(|key| key < target);
            let exact = keys.get(first_at_or_above) == Some(target);
            prop_assert_eq!(node.find(target, &load).unwrap(), (first_at_or_above, exact));
        }
    }

    #[test]
    fn a_fixed_leaf_finds_the_first_key_at_or_above_the_target(
        keys in proptest::collection::btree_set(vec(0u8..4, 9), 1..30),
        targets in vec(key_strategy(), 1..20),
    ) {
        let keys: Vec<Vec<u8>> = keys.into_iter().collect();
        let content = leaf(&keys.iter().map(|key| (key.as_slice(), inline(&[5; 3]))).collect::<Vec<_>>());
        let page = sealed(&content, 0, &mut Vec::new());
        let header = PageHeader::open(SEED, &page, at()).unwrap();
        prop_assert_eq!(header.format, FIXED);
        let node = Node::parse(&header, &page, TABLE).unwrap();
        let load = long_key_loader(&[]);
        for target in targets.iter().chain(&keys) {
            let first_at_or_above = keys.partition_point(|key| key < target);
            let exact = keys.get(first_at_or_above) == Some(target);
            prop_assert_eq!(node.find(target, &load).unwrap(), (first_at_or_above, exact));
        }
    }

    #[test]
    fn a_branch_finds_the_child_whose_range_holds_the_target(
        lows in proptest::collection::btree_set(key_strategy(), 1..30),
        targets in vec(key_strategy(), 1..20),
    ) {
        let lows: Vec<Vec<u8>> = lows.into_iter().filter(|low| !low.is_empty()).collect();
        let mut entries = vec![BranchEntry { low: Bytes::from(&b""[..]), child: ChildRef { at: at(), lsn: 1 } }];
        entries.extend(lows.iter().map(|low| BranchEntry {
            low: Bytes::from(low.as_slice()),
            child: ChildRef { at: at(), lsn: 1 },
        }));
        let mut long_keys = Vec::new();
        let page = sealed(&Content::Branch(entries), 1, &mut long_keys);
        let header = PageHeader::open(SEED, &page, at()).unwrap();
        let node = Node::parse(&header, &page, TABLE).unwrap();
        let load = long_key_loader(&long_keys);
        for target in targets.iter().chain(&lows) {
            let child = lows.partition_point(|low| low <= target);
            prop_assert_eq!(node.search(target, &load).unwrap(), child);
        }
    }
}

#[test]
fn every_inline_key_of_a_variable_page_takes_the_direct_comparison() {
    let keys: Vec<Vec<u8>> = (0u8..40).map(|key| vec![7, 7, key, key % 3]).collect();
    let content = leaf(
        &keys
            .iter()
            .map(|key| (key.as_slice(), inline(&vec![0; usize::from(key[2] % 4)])))
            .collect::<Vec<_>>(),
    );
    let page = sealed(&content, 0, &mut Vec::new());
    let header = PageHeader::open(SEED, &page, at()).unwrap();
    let node = Node::parse(&header, &page, TABLE).unwrap();
    let Layout::Variable { offsets } = node.layout else {
        panic!("the page should use the variable format");
    };
    assert_eq!(node.cells(), keys.len());
    for (index, key) in keys.iter().enumerate() {
        let range = node.inline_key_range(offsets, index).unwrap();
        assert_eq!(&[node.prefix, &page[range]].concat(), key);
    }
}
