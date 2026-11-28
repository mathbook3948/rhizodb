//! Reports where the bytes of the database file `RHIZODB_SIZE_FILE` names go, by table, page
//! fill, and property key. `mise run size-report` runs it on the benchmark graph. It opens the
//! file as a writer, so give it a copy.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::engine::any_fs::open_path;
use crate::engine::cache::Access;
use crate::engine::layout::BLOCK;
use crate::engine::layout::TableId;
use crate::engine::memtable::Bytes;
use crate::engine::node::CellValue;
use crate::engine::node::ChildRef;
use crate::engine::node::Node;
use crate::engine::node::PageSize;
use crate::engine::node::StoredValue;
use crate::engine::node::value_bytes;
use crate::engine::pages::PageSource;
use crate::engine::store::EngineOptions;
use crate::engine::tree::fetch;
use crate::storage::Maintenance;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::Table;

const PROPERTY_KEY_TAG: u8 = 2;

#[derive(Debug, Default)]
struct Sizes {
    entries: u64,
    key_bytes: u64,
    inline_value_bytes: u64,
    external_value_bytes: u64,
    value_runs: u64,
    value_run_blocks: u64,
    leaves: u64,
    branches: u64,
    leaf_used: u64,
    by_start: BTreeMap<Vec<u8>, (u64, u64, u64)>,
}

fn walk(source: &dyn PageSource, table: TableId, page: ChildRef, level: u8, sizes: &mut Sizes) {
    let cached = fetch(source, table, page, level, Access::Point).unwrap();
    let node = Node::parse(&cached.header, &cached.bytes, table).unwrap();
    if !node.is_leaf() {
        sizes.branches += 1;
        for index in 0..node.cells() {
            walk(source, table, node.child(index).unwrap(), level - 1, sizes);
        }
        return;
    }
    sizes.leaves += 1;
    let mut size = PageSize::default();
    let mut used = 0;
    let mut start = None;
    for index in 0..node.cells() {
        let key = node.key(index).unwrap().full(&|_| Ok(Vec::new())).unwrap();
        sizes.entries += 1;
        if start.is_none() {
            start = Some(page_class(table, &key));
        }
        sizes.key_bytes += key.len() as u64;
        let value = match node.value(index).unwrap() {
            CellValue::Inline(value) => {
                sizes.inline_value_bytes += value.len() as u64;
                StoredValue::Inline(Bytes::from(value))
            }
            CellValue::External(blob) => {
                sizes.external_value_bytes += blob.length;
                sizes.value_runs += 1;
                sizes.value_run_blocks += u64::from(blob.first.blocks);
                StoredValue::External(blob)
            }
        };
        (size, used) = size.with(Some(&Bytes::from(key.as_slice())), value_bytes(&value));
    }
    sizes.leaf_used += used as u64;
    let class = sizes.by_start.entry(start.unwrap_or_default()).or_default();
    class.0 += 1;
    class.1 += used as u64;
    class.2 += node.cells() as u64;
}

fn page_class(table: TableId, key: &[u8]) -> Vec<u8> {
    if table == TableId::of(Table::NodeColumns) && key.first() == Some(&1) {
        let group_len = usize::from(key[1]);
        let token_len = usize::from(key[2 + group_len]);
        return key[3 + group_len..3 + group_len + token_len].to_vec();
    }
    if table == TableId::of(Table::Adjacency) {
        return key[..1].to_vec();
    }
    Vec::new()
}

fn mib(bytes: u64) -> String {
    format!("{:.3}", bytes as f64 / (1024.0 * 1024.0))
}

#[test]
#[ignore = "reports on the file RHIZODB_SIZE_FILE names"]
fn size_report() {
    let Some(path) = std::env::var_os("RHIZODB_SIZE_FILE").map(PathBuf::from) else {
        return;
    };
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    let engine = open_path(&path, EngineOptions::default()).unwrap();
    let stats = engine.stats().unwrap();
    println!(
        "file {} MiB, live {} MiB, live extents {}, free extents {}",
        mib(file_bytes),
        mib(stats.live_bytes),
        stats.live_extents,
        stats.free_extents
    );
    let pinned = engine.pin().unwrap();
    let tree = pinned.hold().unwrap();
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7} {:>6} {:>9} {:>6} {:>9}",
        "table",
        "entries",
        "key MiB",
        "inl MiB",
        "ext MiB",
        "runblk",
        "leaves",
        "branch",
        "disk MiB",
        "fill",
        "B/entry"
    );
    let mut total = 0;
    for (table, root) in &tree.roots {
        let mut sizes = Sizes::default();
        walk(&*tree.source, *table, root.page, root.level, &mut sizes);
        let disk = (sizes.leaves + sizes.branches + sizes.value_run_blocks) * BLOCK as u64;
        total += disk;
        let name = table
            .table()
            .map_or_else(|_| format!("{:#x}", table.0), |table| format!("{table:?}"));
        println!(
            "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7} {:>6} {:>9} {:>6.3} {:>9.1}",
            name,
            sizes.entries,
            mib(sizes.key_bytes),
            mib(sizes.inline_value_bytes),
            mib(sizes.external_value_bytes),
            sizes.value_run_blocks,
            sizes.leaves,
            sizes.branches,
            mib(disk),
            sizes.leaf_used as f64 / (sizes.leaves.max(1) * BLOCK as u64) as f64,
            disk as f64 / sizes.entries.max(1) as f64,
        );
        if sizes.by_start.len() > 1 {
            for (class, (pages, used, cells)) in &sizes.by_start {
                println!(
                    "    {class:?}: {pages} leaves, fill {:.3}, {:.1} cells a page",
                    *used as f64 / (*pages * BLOCK as u64) as f64,
                    *cells as f64 / *pages as f64
                );
            }
        }
    }
    println!("tables total {} MiB", mib(total));
    drop(tree);
    drop(pinned);
    columns_by_property(&engine);
    engine.close().unwrap();
}

fn columns_by_property(engine: &impl Storage) {
    let read = engine.begin_read().unwrap();
    let mut names = BTreeMap::new();
    for (key, value) in read.scan_prefix(Table::TokenById, &[]).unwrap() {
        names.insert(key, String::from_utf8_lossy(&value).into_owned());
    }
    let mut by_token: BTreeMap<u32, (u64, u64, u64)> = BTreeMap::new();
    let mut deltas = (0u64, 0u64);
    read.visit_prefix::<crate::storage::StorageError>(Table::NodeColumns, &[], |key, value| {
        match key[0] {
            1 => {
                let group_len = usize::from(key[1]);
                let token_len = usize::from(key[2 + group_len]);
                let token = key[3 + group_len..3 + group_len + token_len]
                    .iter()
                    .fold(0u32, |token, byte| token << 8 | u32::from(*byte));
                let entry = by_token.entry(token).or_default();
                entry.0 += 1;
                entry.1 += key.len() as u64;
                entry.2 += value.len() as u64;
            }
            _ => {
                deltas.0 += 1;
                deltas.1 += value.len() as u64;
            }
        }
        Ok(())
    })
    .unwrap();
    println!("node columns by property key token: parts, key MiB, value MiB");
    for (token, (parts, keys, values)) in by_token {
        let mut key = vec![PROPERTY_KEY_TAG];
        key.extend_from_slice(&token.to_be_bytes());
        let name = names.get(&key).map_or("?", String::as_str);
        println!(
            "  {token:>4} {name:<24} {parts:>8} {:>9} {:>9}",
            mib(keys),
            mib(values)
        );
    }
    println!("  deltas {} values {} MiB", deltas.0, mib(deltas.1));
    for table in [
        Table::Adjacency,
        Table::RelationshipChunks,
        Table::NodeChunks,
        Table::PropertyEntries,
        Table::Indexes,
    ] {
        let mut count = 0u64;
        let mut values = 0u64;
        let mut keys = 0u64;
        let mut lengths: BTreeMap<usize, u64> = BTreeMap::new();
        read.visit_prefix::<crate::storage::StorageError>(table, &[], |key, value| {
            count += 1;
            keys += key.len() as u64;
            values += value.len() as u64;
            *lengths.entry(key.len()).or_default() += 1;
            Ok(())
        })
        .unwrap();
        println!(
            "{table:?}: {count} entries, keys {} MiB, values {} MiB, key lengths {lengths:?}",
            mib(keys),
            mib(values)
        );
    }
}
