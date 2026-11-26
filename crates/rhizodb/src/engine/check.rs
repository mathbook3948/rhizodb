use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashSet;

use super::blob::BlobRef;
use super::blob::Role;
use super::blob::RunRef;
use super::blob::decode_run;
use super::cache::Access;
use super::catalog::Root;
use super::error::EngineError;
use super::extent::Blocks;
use super::extent::ExtentTable;
use super::header::Header;
use super::layout::BlockRef;
use super::layout::ExtentId;
use super::layout::TableId;
use super::node::CellValue;
use super::node::ChildRef;
use super::node::Node;
use super::pages::PageSource;
use super::pages::load_blob;
use super::tree::Tree;
use super::tree::fetch;

/// What a check of the database file found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CheckReport {
    pub(crate) entries: BTreeMap<TableId, u64>,
    pub(crate) pages: u64,
    pub(crate) runs: u64,
    pub(crate) live_blocks: u64,
}

struct Checker<'a> {
    source: &'a dyn PageSource,
    header: &'a Header,
    extents: &'a ExtentTable,
    reached: HashSet<BlockRef>,
    report: CheckReport,
}

fn problem(message: String) -> EngineError {
    EngineError::corrupt(format!("check: {message}"))
}

/// Walks every tree from the durable header and fails on the first broken invariant: a page or
/// run that fails its checks, keys out of order or outside their parent's range, a table whose
/// entry count differs from its root's, a block reached twice or past its extent's written
/// length, or an extent whose live blocks differ from the blocks reached in it.
pub(crate) fn check(
    header: &Header,
    extents: &ExtentTable,
    catalog: &[RunRef],
    tree: &Tree,
) -> Result<CheckReport, EngineError> {
    let (mut reached, mut report) = walk(header, extents, catalog, tree)?;
    for (id, entry) in &extents.entries {
        let found = reached.remove(id).unwrap_or_default();
        if found != entry.used {
            return Err(problem(format!(
                "extent {} records {} live blocks at {:?} but {} at {:?} are reachable",
                id.0,
                entry.live(),
                entry.used,
                found.count(),
                found
            )));
        }
        report.live_blocks += u64::from(found.count());
    }
    if let Some(id) = reached.keys().next() {
        return Err(problem(format!(
            "blocks of extent {} are reachable but it is not in the table",
            id.0
        )));
    }
    Ok(report)
}

/// The blocks of each extent that the durable header reaches, found by the walk [`check`] makes.
pub(crate) fn reachable(
    header: &Header,
    extents: &ExtentTable,
    catalog: &[RunRef],
    tree: &Tree,
) -> Result<BTreeMap<ExtentId, Blocks>, EngineError> {
    Ok(walk(header, extents, catalog, tree)?.0)
}

fn walk(
    header: &Header,
    extents: &ExtentTable,
    catalog: &[RunRef],
    tree: &Tree,
) -> Result<(BTreeMap<ExtentId, Blocks>, CheckReport), EngineError> {
    let mut checker = Checker {
        source: &*tree.source,
        header,
        extents,
        reached: HashSet::new(),
        report: CheckReport::default(),
    };
    for run in catalog {
        checker.reach_run(*run)?;
    }
    for (table, root) in &tree.roots {
        checker.table(*table, root)?;
    }
    let mut reached: BTreeMap<ExtentId, Blocks> = BTreeMap::new();
    for at in &checker.reached {
        let blocks = reached.entry(at.extent()).or_default();
        *blocks = blocks.with(&Blocks::range(at.block(), 1));
    }
    Ok((reached, checker.report))
}

impl Checker<'_> {
    fn reach(&mut self, at: BlockRef, blocks: u32) -> Result<(), EngineError> {
        let entry = self
            .extents
            .entries
            .get(&at.extent())
            .ok_or_else(|| problem(format!("{at:?} lies outside every extent")))?;
        if at
            .block()
            .checked_add(blocks)
            .is_none_or(|end| end > entry.written)
        {
            return Err(problem(format!(
                "{blocks} blocks at {at:?} reach past the extent's {} written blocks",
                entry.written
            )));
        }
        for offset in 0..blocks {
            if !self.reached.insert(at.offset(offset)?) {
                return Err(problem(format!(
                    "block {offset} of {at:?} is reached twice"
                )));
            }
        }
        Ok(())
    }

    fn reach_run(&mut self, run: RunRef) -> Result<(), EngineError> {
        if run.lsn > self.header.tree_lsn() {
            return Err(problem(format!(
                "the run at {:?} is newer than the checkpoint",
                run.at
            )));
        }
        self.report.runs += 1;
        self.reach(run.at, run.blocks)
    }

    fn reach_blob(&mut self, blob: &BlobRef, role: Role) -> Result<Vec<u8>, EngineError> {
        let bytes = load_blob(self.source, blob, role)?;
        let mut run = blob.first;
        loop {
            self.reach_run(run)?;
            let blocks = self.source.run_bytes(run)?;
            let (_, decoded) = decode_run(self.source.seed(), &blocks, run, role)?;
            if decoded.next.at.is_null() {
                return Ok(bytes);
            }
            run = decoded.next;
        }
    }

    fn table(&mut self, table: TableId, root: &Root) -> Result<(), EngineError> {
        let mut entries = 0;
        self.node(table, root.page, root.level, &[], None, &mut entries)?;
        if entries != root.entries {
            return Err(problem(format!(
                "table {:#x} holds {entries} entries but its root counts {}",
                table.0, root.entries
            )));
        }
        self.report.entries.insert(table, entries);
        Ok(())
    }

    fn node(
        &mut self,
        table: TableId,
        page: ChildRef,
        level: u8,
        low: &[u8],
        high: Option<&[u8]>,
        entries: &mut u64,
    ) -> Result<(), EngineError> {
        let cached = fetch(self.source, table, page, level, Access::Point)?;
        if cached.header.lsn > self.header.tree_lsn()
            || cached.header.generation > self.header.generation
        {
            return Err(problem(format!(
                "the page at {:?} is newer than the header",
                page.at
            )));
        }
        self.reach(page.at, 1)?;
        self.report.pages += 1;
        let node = Node::parse(&cached.header, &cached.bytes, table)?;
        let first = usize::from(!node.is_leaf());
        let mut keys = Vec::with_capacity(node.cells());
        for index in first..node.cells() {
            let cell = node.key(index)?;
            let key = match cell.blob() {
                Some(blob) => {
                    let blob = *blob;
                    let key = self.reach_blob(&blob, Role::Key)?;
                    let loaded = cell.full(&|_| Ok(key.clone()))?;
                    if loaded != key {
                        return Err(problem("a long key differs from its run".to_owned()));
                    }
                    key
                }
                None => cell.full(&|_| Err(problem("an inline key needed a run".to_owned())))?,
            };
            if keys
                .last()
                .is_some_and(|before: &Vec<u8>| before.as_slice() >= key.as_slice())
            {
                return Err(problem(format!(
                    "the keys of the page at {:?} are out of order",
                    page.at
                )));
            }
            let below = key.as_slice().cmp(low) == Ordering::Less;
            let above = high.is_some_and(|high| key.as_slice() >= high);
            let at_low = index == first && !node.is_leaf() && key.as_slice() == low;
            if below || above || at_low {
                return Err(problem(format!(
                    "a key of the page at {:?} lies outside its parent's range",
                    page.at
                )));
            }
            keys.push(key);
        }
        if node.is_leaf() {
            for index in 0..node.cells() {
                if let CellValue::External(blob) = node.value(index)? {
                    self.reach_blob(&blob, Role::Value)?;
                }
            }
            *entries += node.cells() as u64;
            return Ok(());
        }
        for index in 0..node.cells() {
            let child_low = if index == 0 {
                low
            } else {
                keys[index - 1].as_slice()
            };
            let child_high = match keys.get(index) {
                Some(next) => Some(next.as_slice()),
                None => high,
            };
            let child = node.child(index)?;
            self.node(table, child, level - 1, child_low, child_high, entries)?;
        }
        Ok(())
    }
}
