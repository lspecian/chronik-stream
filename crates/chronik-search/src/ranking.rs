//! One order for search hits, everywhere a search assembles them.
//!
//! A search answer is put together from many sources: the hot index (one
//! Tantivy index per partition), every archived segment, and every node of the
//! cluster. Paging (`from`/`size`) is only correct if each source returns its
//! top `from + size` hits in the SAME order the final merge uses, and the
//! final merge skips `from` once. Before v2.17.3 each archived segment skipped
//! its own first `from` hits, the hot index ignored `from`, nobody skipped it on
//! the merged list, and ties (every hit of `match_all`) were broken
//! differently by every source — so page 2 repeated page 1 and dropped records
//! that belonged on it.
//!
//! The order: score (highest first), then timestamp (newest first), then
//! topic, partition, offset (highest first), then `_id` — total, so the same
//! request always yields the same pages.

use crate::api::Hit;
use std::cmp::Ordering;
use tantivy::collector::TopDocs;
use tantivy::query::Query;
use tantivy::{DocAddress, DocId, Score, Searcher, SegmentReader};

use crate::corpus_stats::CorpusStats;
use crate::sort::SortSpec;

/// The per-document rank inside one Tantivy index; compared field by field,
/// highest wins (TopDocs keeps the largest keys). `neg_partition` puts lower
/// partitions first among equal timestamps, as [`hit_order`] does.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub(crate) struct RankKey {
    pub(crate) score: Score,
    pub(crate) timestamp_s: i64,
    pub(crate) neg_partition: i64,
    pub(crate) offset: i64,
}

/// The least `(score, second)` a document must reach to be worth ranking: the
/// `window`-th best another source already holds. A document strictly below
/// it on score, or on the same score strictly older, cannot reach the page
/// whatever its topic, partition or offset (they only break ties within one
/// second), so collectors pass it over before building its key.
pub(crate) type Floor = (Score, i64);

fn below_floor(floor: Option<Floor>, score: Score, second: impl FnOnce() -> i64) -> (bool, Option<i64>) {
    match floor {
        Some((s, _)) if score < s => (true, None),
        Some((s, t)) if score == s => {
            let ts = second();
            (ts < t, Some(ts))
        }
        _ => (false, None),
    }
}

/// The fast fields an index keeps the rank in, found from its schema.
#[derive(Clone, Copy)]
struct RankFields {
    timestamp: &'static str,
    /// `timestamp` is a date field (else i64 milliseconds).
    timestamp_is_date: bool,
    offset: &'static str,
    /// Absent when the index holds a single partition.
    partition: Option<&'static str>,
    /// Every segment keeps its documents highest offset first (an archive
    /// written with `archive_settings`; one partition): a document scoring
    /// the same as an earlier one ranks higher only with a newer timestamp
    /// second, so once no document still to come is newer
    /// ([`RankColumns::newest_from`]) ties stop being read.
    newest_first: bool,
}

impl RankFields {
    /// The layouts written by Chronik, newest first: an archived segment
    /// (`_ts`, `_offset`, one partition), a hot partition index (`timestamp`,
    /// `offset`, one partition), and a leftover RealtimeIndexer index
    /// (`_timestamp`, `_offset`, `_partition` — all partitions of a topic).
    fn detect(schema: &tantivy::schema::Schema) -> Option<Self> {
        let fast = |name: &str| {
            schema.get_field(name).map(|f| schema.get_field_entry(f).is_fast()).unwrap_or(false)
        };
        let candidates = [
            RankFields {
                timestamp: ARCHIVE_TIMESTAMP,
                timestamp_is_date: true,
                offset: "_offset",
                partition: None,
                newest_first: false,
            },
            RankFields {
                timestamp: "_timestamp",
                timestamp_is_date: true,
                offset: "_offset",
                partition: Some("_partition"),
                newest_first: false,
            },
            RankFields {
                timestamp: "timestamp",
                timestamp_is_date: false,
                offset: "offset",
                partition: None,
                newest_first: false,
            },
        ];
        candidates.into_iter().find(|c| {
            fast(c.timestamp) && fast(c.offset) && c.partition.map_or(true, |p| fast(p))
        })
    }

    /// [`Self::detect`], noting whether the index keeps documents newest first.
    fn of(searcher: &Searcher) -> Option<Self> {
        let fields = Self::detect(searcher.schema())?;
        let newest_first = fields.timestamp == ARCHIVE_TIMESTAMP
            && fields.partition.is_none()
            && searcher
                .index()
                .settings()
                .sort_by_field
                .as_ref()
                .is_some_and(|s| s.field == fields.offset && s.order == tantivy::Order::Desc);
        Some(Self { newest_first, ..fields })
    }
}

/// A column's values by document. When every document has one (always, in
/// the indexes Chronik writes) its values are read directly; the default-value
/// wrapper looked each one up through the column's optional index, about a
/// third of the CPU of a broad query (one value per matching document, three
/// columns).
fn dense<T>(col: tantivy::columnar::Column<T>, default: T) -> std::sync::Arc<dyn tantivy::columnar::ColumnValues<T>>
where
    T: tantivy::columnar::MonotonicallyMappableToU64 + PartialOrd + Copy + Send + Sync + std::fmt::Debug + 'static,
{
    if col.get_cardinality() == tantivy::columnar::Cardinality::Full {
        col.values
    } else {
        col.first_or_default_col(default)
    }
}

/// The top `limit` documents of `query` in the order of [`hit_order`].
///
/// Falls back to plain relevance when the index is of no layout Chronik
/// writes (a REST-created index), which is still deterministic per index.
pub fn top_docs(
    searcher: &Searcher,
    query: &dyn Query,
    limit: usize,
) -> tantivy::Result<Vec<(Score, DocAddress)>> {
    top_docs_with(searcher, query, limit, None)
}

/// [`top_docs`] scored with `stats` (the statistics of every index the search
/// reads) instead of this index's own.
pub fn top_docs_with(
    searcher: &Searcher,
    query: &dyn Query,
    limit: usize,
    stats: Option<&CorpusStats>,
) -> tantivy::Result<Vec<(Score, DocAddress)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    match ranked_docs(searcher, query, limit, None, stats)? {
        Some(ranked) => Ok(ranked.into_iter().map(|(key, addr)| (key.score, addr)).collect()),
        None => search_scored(searcher, query, &TopDocs::with_limit(limit).order_by_score(), stats),
    }
}

pub(crate) fn search_scored<C: tantivy::collector::Collector>(
    searcher: &Searcher,
    query: &dyn Query,
    collector: &C,
    stats: Option<&CorpusStats>,
) -> tantivy::Result<C::Fruit> {
    match stats {
        Some(stats) => searcher.search_with_statistics_provider(query, collector, &stats.provider(searcher.schema())),
        None => searcher.search(query, collector),
    }
}

/// [`top_docs`] with each document's whole rank key, for a caller that merges
/// several indexes before reading any document; documents ranking below
/// `floor` are left out. `None` when the index is of no layout Chronik writes
/// (no rank columns): use [`top_docs`].
pub(crate) fn ranked_docs(
    searcher: &Searcher,
    query: &dyn Query,
    limit: usize,
    floor: Option<Floor>,
    stats: Option<&CorpusStats>,
) -> tantivy::Result<Option<Vec<(RankKey, DocAddress)>>> {
    let Some(fields) = RankFields::of(searcher) else {
        return Ok(None);
    };
    if limit == 0 {
        return Ok(Some(Vec::new()));
    }
    if query.downcast_ref::<tantivy::query::AllQuery>().is_some() {
        return Ok(Some(newest_of_all(searcher, fields, limit, floor)));
    }
    Ok(Some(
        search_scored(searcher, query, &RankCollector { fields, limit, floor }, stats)?
            .into_iter()
            .map(|held| (held.0, held.1))
            .collect(),
    ))
}

/// A [`RankKey`] with a total order: on equal keys the lower document address
/// ranks higher (as Tantivy's `TopDocs` breaks ties).
#[derive(PartialEq)]
struct Held(RankKey, DocAddress);
impl Eq for Held {}
impl PartialOrd for Held {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Held {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal).then_with(|| other.1.cmp(&self.1))
    }
}

/// The top `limit` documents by [`RankKey`], reading a document's columns
/// only when it could still place: once `limit` are held, a document scoring
/// below the worst of them is passed over on its score alone, and one scoring
/// the same is passed over on its timestamp before partition and offset are
/// read. `TopDocs::tweak_score` built the whole key (three column reads) for
/// every matching document — most of the CPU of a text or term query matching
/// many records.
struct RankCollector {
    fields: RankFields,
    limit: usize,
    floor: Option<Floor>,
}

type Col = std::sync::Arc<dyn tantivy::columnar::ColumnValues<i64>>;

/// The timestamp column of an archived segment's layout ([`RankFields::detect`]).
const ARCHIVE_TIMESTAMP: &str = "_ts";
/// Archived segments whose rank columns are kept open at most.
const RANK_COLUMN_SEGMENTS: usize = 8192;

type RankColumnCache = std::sync::Mutex<(std::collections::HashMap<tantivy::index::SegmentId, (std::sync::Arc<RankColumns>, u64)>, u64)>;

fn rank_column_cache() -> &'static RankColumnCache {
    static CACHE: std::sync::OnceLock<RankColumnCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Drops the cached rank columns of these segments (their archive was closed).
pub(crate) fn forget_segments(ids: impl IntoIterator<Item = tantivy::index::SegmentId>) {
    let mut guard = rank_column_cache().lock().unwrap();
    for id in ids {
        guard.0.remove(&id);
    }
}

/// A segment's rank columns, opened once. Opening a column looks its name up
/// in the segment's column dictionary and decodes its block headers (~400
/// for an archive's timestamp): done for every segment of every archive on
/// every query, it cost about as much as the ranking it served.
struct RankColumns {
    ts: TsColumn,
    offset: Option<Col>,
    partition: Option<Col>,
    /// In a segment kept highest offset first (`RankFields::newest_first`):
    /// per block of `SUFFIX_BLOCK` documents, the newest second of every
    /// document from that block to the end. Built once per segment (one pass
    /// over the timestamp column). Exact rank order would need timestamps
    /// that never rise along the segment; a few records stamped late by one
    /// producer among several (18 in a 3.7M-record archive at 10M) cost only
    /// the blocks around them.
    newest_after: Option<Box<[i64]>>,
}

/// Documents per block of `RankColumns::newest_after`.
const SUFFIX_BLOCK_SHIFT: u32 = 10;

enum TsColumn {
    Date(std::sync::Arc<dyn tantivy::columnar::ColumnValues<tantivy::DateTime>>),
    Millis(Col),
    Missing,
}

impl RankColumns {
    fn open(segment: &SegmentReader, fields: RankFields) -> Self {
        let ff = segment.fast_fields();
        let i64col = |name: &str| ff.i64(name).ok().map(|c| dense(c, 0));
        let ts = if fields.timestamp_is_date {
            match ff.date(fields.timestamp) {
                Ok(c) => TsColumn::Date(dense(c, tantivy::DateTime::from_timestamp_millis(0))),
                Err(_) => TsColumn::Missing,
            }
        } else {
            i64col(fields.timestamp).map_or(TsColumn::Missing, TsColumn::Millis)
        };
        let mut cols =
            RankColumns { ts, offset: i64col(fields.offset), partition: fields.partition.and_then(i64col), newest_after: None };
        if fields.newest_first && !matches!(cols.ts, TsColumn::Missing) {
            let max_doc = segment.max_doc();
            let blocks = (max_doc as usize).div_ceil(1 << SUFFIX_BLOCK_SHIFT);
            let mut newest = vec![i64::MIN; blocks];
            for doc in 0..max_doc {
                let b = (doc >> SUFFIX_BLOCK_SHIFT) as usize;
                newest[b] = newest[b].max(cols.second(doc));
            }
            for b in (0..blocks.saturating_sub(1)).rev() {
                newest[b] = newest[b].max(newest[b + 1]);
            }
            cols.newest_after = Some(newest.into_boxed_slice());
        }
        cols
    }

    /// No document at or after `doc` has a newer second than this. Only for a
    /// segment kept highest offset first (else the segment's newest).
    #[inline]
    fn newest_from(&self, doc: DocId) -> i64 {
        match &self.newest_after {
            Some(n) => n.get((doc >> SUFFIX_BLOCK_SHIFT) as usize).copied().unwrap_or(i64::MIN),
            None => self.newest_second(),
        }
    }

    /// The segment's columns for `fields`, from the cache when it holds them.
    ///
    /// Only archived segments are cached: an archive never changes once
    /// written (deletes only mark documents), so its segment id identifies its
    /// columns, and they are dropped when the search cache closes the archive
    /// ([`forget_segments`]) — kept longer, they would hold its unlinked files.
    /// A hot partition's segments are replaced every commit; opening theirs
    /// each time is what it was before.
    fn of(segment: &SegmentReader, fields: RankFields) -> std::sync::Arc<Self> {
        use std::sync::Arc;
        if fields.timestamp != ARCHIVE_TIMESTAMP {
            return Arc::new(Self::open(segment, fields));
        }
        let key = segment.segment_id();
        {
            let mut guard = rank_column_cache().lock().unwrap();
            let (map, tick) = &mut *guard;
            *tick += 1;
            if let Some((cols, used)) = map.get_mut(&key) {
                *used = *tick;
                return cols.clone();
            }
        }
        let cols = Arc::new(Self::open(segment, fields));
        let mut guard = rank_column_cache().lock().unwrap();
        let (map, tick) = &mut *guard;
        if map.len() >= RANK_COLUMN_SEGMENTS {
            // Archives closed without passing through `forget_segments`
            // (dropped while a search still held them): the least recently
            // used half goes.
            let mut uses: Vec<u64> = map.values().map(|(_, used)| *used).collect();
            let (_, median, _) = uses.select_nth_unstable(map.len() / 2);
            let median = *median;
            map.retain(|_, (_, used)| *used > median);
        }
        map.insert(key, (cols.clone(), *tick));
        cols
    }

    #[inline]
    fn second(&self, doc: DocId) -> i64 {
        match &self.ts {
            TsColumn::Date(c) => c.get_val(doc).into_timestamp_secs(),
            TsColumn::Millis(c) => c.get_val(doc).div_euclid(1000),
            TsColumn::Missing => 0,
        }
    }

    fn newest_second(&self) -> i64 {
        match &self.ts {
            TsColumn::Date(c) => c.max_value().into_timestamp_secs(),
            TsColumn::Millis(c) => c.max_value().div_euclid(1000),
            TsColumn::Missing => 0,
        }
    }

    #[inline]
    fn offset(&self, doc: DocId) -> i64 {
        self.offset.as_ref().map_or(0, |c| c.get_val(doc))
    }

    #[inline]
    fn partition(&self, doc: DocId) -> i64 {
        self.partition.as_ref().map_or(0, |c| c.get_val(doc))
    }
}

struct RankSegmentCollector {
    ord: tantivy::SegmentOrdinal,
    limit: usize,
    floor: Option<Floor>,
    /// No document still to come is newer than this second: the segment's
    /// newest, and in a segment kept highest offset first, the newest from
    /// the last document collected on ([`RankColumns::newest_from`]). A
    /// document scoring the same as the floor cannot place once the floor is
    /// newer.
    bound_second: i64,
    /// The segment is kept highest offset first: every document still to
    /// come has a lower offset than every one held, so one scoring the same
    /// as the worst held beats it only with a newer second.
    newest_first: bool,
    cols: std::sync::Arc<RankColumns>,
    /// The best held; the root is the worst of them.
    best: std::collections::BinaryHeap<std::cmp::Reverse<Held>>,
}

impl tantivy::collector::Collector for RankCollector {
    type Fruit = Vec<Held>;
    type Child = RankSegmentCollector;

    fn for_segment(&self, ord: tantivy::SegmentOrdinal, segment: &SegmentReader) -> tantivy::Result<Self::Child> {
        let cols = RankColumns::of(segment, self.fields);
        Ok(RankSegmentCollector {
            ord,
            limit: self.limit,
            floor: self.floor,
            bound_second: cols.newest_second(),
            newest_first: cols.newest_after.is_some(),
            cols,
            best: std::collections::BinaryHeap::with_capacity(self.limit + 1),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    /// Lets the query skip postings blocks that cannot beat what is held
    /// (Block-WAND), as `TopDocs` does. Needs the Tantivy fork: upstream 0.27
    /// stores zero block maxima for fields without fieldnorms, the JSON field
    /// among them, and pruning on those drops real hits (quickwit-oss/tantivy
    /// #3230).
    fn collect_segment(
        &self,
        weight: &dyn tantivy::query::Weight,
        ord: tantivy::SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<Vec<Held>> {
        use tantivy::collector::SegmentCollector;
        let mut child = self.for_segment(ord, segment)?;
        let alive = segment.alive_bitset();
        weight.for_each_pruning(child.threshold(), segment, &mut |doc, score| {
            if !alive.is_some_and(|a| a.is_deleted(doc)) {
                child.collect(doc, score);
            }
            child.threshold()
        })?;
        Ok(child.harvest())
    }

    fn merge_fruits(&self, fruits: Vec<Vec<Held>>) -> tantivy::Result<Vec<Held>> {
        let mut all: Vec<Held> = fruits.into_iter().flatten().collect();
        all.sort_unstable_by(|a, b| b.cmp(a));
        all.truncate(self.limit);
        Ok(all)
    }
}

impl RankSegmentCollector {
    /// The score a document must exceed to be passed to `collect`: just below
    /// the least one that can still place, since an equal score places when
    /// its record is newer; that score itself where no record still to come
    /// can be newer (`bound_second`, `newest_first`).
    fn threshold(&self) -> Score {
        let floor = self.floor.map(|(s, t)| if self.bound_second < t { s } else { s.next_down() });
        let held = (self.best.len() == self.limit).then(|| {
            let worst = &self.best.peek().expect("limit > 0").0 .0;
            if self.newest_first && self.bound_second <= worst.timestamp_s {
                worst.score
            } else {
                worst.score.next_down()
            }
        });
        floor.into_iter().chain(held).fold(Score::MIN, Score::max)
    }
}

impl tantivy::collector::SegmentCollector for RankSegmentCollector {
    type Fruit = Vec<Held>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let mut timestamp_s = None;
        if self.newest_first {
            self.bound_second = self.cols.newest_from(doc);
        }
        if let Some((s, t)) = self.floor {
            if score < s {
                return;
            }
            if score == s && (self.bound_second < t || *timestamp_s.get_or_insert_with(|| self.cols.second(doc)) < t) {
                return;
            }
        }
        if self.best.len() == self.limit {
            let worst = &self.best.peek().expect("limit > 0").0 .0;
            if score < worst.score {
                return;
            }
            if score == worst.score {
                let ts = timestamp_s.unwrap_or_else(|| self.cols.second(doc));
                if ts < worst.timestamp_s {
                    return;
                }
                timestamp_s = Some(ts);
            }
        }
        let key = RankKey {
            score,
            timestamp_s: timestamp_s.unwrap_or_else(|| self.cols.second(doc)),
            neg_partition: -self.cols.partition(doc),
            offset: self.cols.offset(doc),
        };
        let held = Held(key, DocAddress::new(self.ord, doc));
        if self.best.len() < self.limit {
            self.best.push(std::cmp::Reverse(held));
        } else if let Some(mut worst) = self.best.peek_mut() {
            // Replaced in place: one sift instead of a pop and a push.
            if held > worst.0 {
                *worst = std::cmp::Reverse(held);
            }
        }
    }

    fn harvest(self) -> Vec<Held> {
        self.best.into_iter().map(|r| r.0).collect()
    }
}

/// The top `limit` documents of an index for `match_all`, in rank order,
/// without reading every document.
///
/// Every document scores the same (1.0, as Tantivy's all-query scores), so
/// the order is timestamp, partition, offset. Segments are visited newest
/// first by the largest timestamp their column holds, and once `limit`
/// documents are held, a segment whose newest record is older than the
/// `limit`-th one cannot place any (equal seconds are still visited: the tie
/// is broken by partition and offset). An archive's segments cover
/// consecutive offset ranges, so typically one segment is read instead of all
/// of them: ranking every document of a 1M-record topic was ~10 ms of CPU per
/// `match_all`.
fn newest_of_all(searcher: &Searcher, fields: RankFields, limit: usize, floor: Option<Floor>) -> Vec<(RankKey, DocAddress)> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    /// RankKey with a total order (scores here are all 1.0).
    #[derive(PartialEq)]
    struct Ranked(RankKey, DocAddress);
    impl Eq for Ranked {}
    impl PartialOrd for Ranked {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Ranked {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal).then_with(|| other.1.cmp(&self.1))
        }
    }

    struct Seg {
        ord: u32,
        /// The best key any document of the segment can have.
        bound: RankKey,
        cols: std::sync::Arc<RankColumns>,
    }
    let mut segments: Vec<Seg> = Vec::new();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        let cols = RankColumns::of(reader, fields);
        let bound = RankKey {
            score: 1.0,
            timestamp_s: cols.newest_second(),
            neg_partition: -cols.partition.as_ref().map_or(0, |c| c.min_value()),
            offset: cols.offset.as_ref().map_or(i64::MAX, |c| c.max_value()),
        };
        segments.push(Seg { ord: ord as u32, bound, cols });
    }
    segments.sort_by(|a, b| b.bound.partial_cmp(&a.bound).unwrap_or(Ordering::Equal).then(a.ord.cmp(&b.ord)));
    // Every document here scores 1.0: past a floor scoring higher none can
    // place; at 1.0 the floor's second bounds which are worth reading.
    let oldest_s = match floor {
        Some((s, _)) if s > 1.0 => return Vec::new(),
        Some((s, t)) if s == 1.0 => t,
        _ => i64::MIN,
    };

    // The `limit` best seen so far; the root is the worst of them.
    let mut best: BinaryHeap<Reverse<Ranked>> = BinaryHeap::with_capacity(limit + 1);
    let readers = searcher.segment_readers();
    for seg in &segments {
        if seg.bound.timestamp_s < oldest_s || (best.len() == limit && best.peek().is_some_and(|w| seg.bound < w.0 .0)) {
            break; // no later segment can do better
        }
        // Newest documents first: records are mostly added in time order, so
        // walking from the newest fills `best` at once and every older
        // document is then passed over on one comparison. From the oldest,
        // each newer document replaced one held (a heap pop and push per
        // document). In a segment kept highest offset first the newest come
        // first, and the walk stops once no document from here on can place:
        // none is newer than the worst held, nor as new with a higher offset
        // (offsets only fall from here). The worst held may come from another
        // segment, so its offset is compared rather than assumed higher.
        let reader = &readers[seg.ord as usize];
        let alive = reader.alive_bitset();
        let ordered = seg.cols.newest_after.is_some();
        let docs: Box<dyn Iterator<Item = DocId>> =
            if fields.newest_first { Box::new(0..reader.max_doc()) } else { Box::new((0..reader.max_doc()).rev()) };
        for doc in docs {
            if ordered {
                let newest = seg.cols.newest_from(doc);
                let beaten = best.len() == limit
                    && best.peek().is_some_and(|w| {
                        let w = &w.0 .0;
                        newest < w.timestamp_s || (newest == w.timestamp_s && seg.cols.offset(doc) < w.offset)
                    });
                if newest < oldest_s || beaten {
                    break;
                }
            }
            if alive.is_some_and(|a| a.is_deleted(doc)) {
                continue;
            }
            let timestamp_s = seg.cols.second(doc);
            if timestamp_s < oldest_s
                || (best.len() == limit && best.peek().is_some_and(|w| timestamp_s < w.0 .0.timestamp_s))
            {
                continue;
            }
            let key = RankKey {
                score: 1.0,
                timestamp_s,
                neg_partition: -seg.cols.partition(doc),
                offset: seg.cols.offset(doc),
            };
            let ranked = Ranked(key, DocAddress::new(seg.ord, doc));
            if best.len() < limit {
                best.push(Reverse(ranked));
            } else if let Some(mut worst) = best.peek_mut() {
                if ranked > worst.0 {
                    *worst = Reverse(ranked);
                }
            }
        }
    }
    let mut out: Vec<Ranked> = best.into_iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.cmp(a));
    out.into_iter().map(|r| (r.0, r.1)).collect()
}

/// A hit's timestamp in whole seconds, the granularity the order uses.
///
/// Archived and RealtimeIndexer indexes keep their timestamps in Tantivy date
/// fast fields, which store seconds; ranking inside them by the second while
/// the merge compared milliseconds made the two orders disagree whenever
/// records shared a second (every producer batch), and pages overlapped. So
/// every source and the merge rank by the second, and partition and offset
/// break the tie. `_source.timestamp` itself stays in milliseconds.
pub fn rank_second(hit: &Hit) -> Option<i64> {
    hit._source.get("timestamp").and_then(|v| v.as_i64()).map(|ms| ms.div_euclid(1000))
}

/// The order of hits across sources: see the module docs. Hits whose
/// `_source` lacks `timestamp`/`partition`/`offset` (documents of REST-created
/// indices) fall through to `_index` and `_id`.
pub fn hit_order(a: &Hit, b: &Hit) -> Ordering {
    fn num(h: &Hit, k: &str) -> Option<i64> {
        h._source.get(k).and_then(|v| v.as_i64())
    }
    let score = |h: &Hit| h._score.unwrap_or(0.0);
    score(b)
        .total_cmp(&score(a))
        .then_with(|| rank_second(b).cmp(&rank_second(a)))
        .then_with(|| a._index.cmp(&b._index))
        .then_with(|| num(a, "partition").cmp(&num(b, "partition")))
        .then_with(|| num(b, "offset").cmp(&num(a, "offset")))
        .then_with(|| a._id.cmp(&b._id))
}

/// Largest `from + size` a search accepts (`CHRONIK_SEARCH_MAX_RESULT_WINDOW`,
/// default 10,000 — Elasticsearch's `index.max_result_window`). Every source
/// collects that many hits, on every node, so a deep page is costly for the
/// whole cluster; beyond it a client should page with a filter instead.
pub fn max_result_window() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("CHRONIK_SEARCH_MAX_RESULT_WINDOW")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(10_000)
    })
}

/// `Err(reason)` when `from + size` is over [`max_result_window`], worded as
/// Elasticsearch words it.
pub fn check_result_window(from: usize, size: usize) -> Result<(), String> {
    let window = from.saturating_add(size);
    let max = max_result_window();
    if window > max {
        return Err(format!(
            "Result window is too large, from + size must be less than or equal to: [{max}] but was [{window}]. \
             See CHRONIK_SEARCH_MAX_RESULT_WINDOW."
        ));
    }
    Ok(())
}

/// One page of `hits`: in [`order_hits`] order, each record once (by `_index`
/// and `_id`, keeping its best-ranked copy — a record can come from both a
/// leftover RealtimeIndexer index and an archive, and from every replica),
/// `from` skipped, `size` kept. With a sort that does not use `_score`, hits
/// carry no score, as in Elasticsearch. Returns the page and how many
/// distinct candidates there were.
pub fn page(mut hits: Vec<Hit>, from: usize, size: usize, sort: Option<&SortSpec>) -> (Vec<Hit>, usize) {
    order_hits(&mut hits, sort);
    let mut seen = std::collections::HashSet::with_capacity(hits.len());
    hits.retain(|h| seen.insert((h._index.clone(), h._id.clone())));
    let found = hits.len();
    let scored = sort.is_none_or(SortSpec::scores);
    let page = hits
        .into_iter()
        .skip(from)
        .take(size)
        .map(|mut h| {
            if !scored {
                h._score = None;
            }
            h
        })
        .collect();
    (page, found)
}

/// Hits in the order of the request's `sort` (by their `sort` values), ties
/// and requests without one in [`hit_order`].
pub fn order_hits(hits: &mut [Hit], sort: Option<&SortSpec>) {
    match sort {
        Some(spec) => hits.sort_by(|a, b| {
            spec.compare(a.sort.as_deref().unwrap_or_default(), b.sort.as_deref().unwrap_or_default())
                .then_with(|| hit_order(a, b))
        }),
        None => hits.sort_by(hit_order),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index in insertion order, or highest offset first as archives are
    /// written (`chronik_storage::tantivy_segment::archive_settings`).
    fn new_index(schema: tantivy::schema::Schema, newest_first: bool) -> tantivy::Index {
        if !newest_first {
            return tantivy::Index::create_in_ram(schema);
        }
        let settings = tantivy::IndexSettings {
            sort_by_field: Some(tantivy::IndexSortByField { field: "_offset".to_string(), order: tantivy::Order::Desc }),
            ..Default::default()
        };
        tantivy::Index::builder().schema(schema).settings(settings).create_in_ram().unwrap()
    }

    /// Every record in one second (a burst): offsets break the tie, and the
    /// segment-skipping top-N still matches ranking every document.
    #[test]
    fn match_all_ties_in_one_second_rank_by_offset() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED};
        let mut sb = Schema::builder();
        let ts = sb.add_date_field("_ts", INDEXED | STORED | FAST);
        let off = sb.add_i64_field("_offset", INDEXED | STORED | FAST);
        let index = tantivy::Index::create_in_ram(sb.build());
        let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        // Segments arrive out of offset order, as a WAL can hold batches.
        for seg in [2i64, 0, 3, 1] {
            for o in seg * 500..seg * 500 + 500 {
                let mut d = tantivy::TantivyDocument::new();
                d.add_date(ts, tantivy::DateTime::from_timestamp_millis(7_000 + (o % 900)));
                d.add_i64(off, o);
                w.add_document(d).unwrap();
            }
            w.commit().unwrap();
        }
        let searcher = index.reader().unwrap().searcher();
        let every = tantivy::query::BooleanQuery::new(vec![(
            tantivy::query::Occur::Must,
            Box::new(tantivy::query::AllQuery) as Box<dyn Query>,
        )]);
        for limit in [1, 10, 500, 501, 1999, 2000, 3000] {
            let addrs = |v: Vec<(Score, DocAddress)>| v.into_iter().map(|(_, a)| a).collect::<Vec<_>>();
            assert_eq!(
                addrs(top_docs(&searcher, &tantivy::query::AllQuery, limit).unwrap()),
                addrs(top_docs(&searcher, &every, limit).unwrap()),
                "limit={limit}"
            );
        }
    }

    /// match_all's segment-skipping top-N returns exactly what ranking every
    /// document returns: several segments, timestamps out of order across
    /// and within them, many records sharing a second, deleted documents.
    #[test]
    fn match_all_skips_segments_without_changing_the_answer() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED};
        // sorted: highest offset first, as archives are written; burst: every
        // timestamp rising with the offset, ~140 records a second, a few
        // stamped 3 s late (several producers on one partition); otherwise
        // some records are stamped far in the past.
        for (date, ts_name, off_name, sorted, burst) in
            [
                (true, "_ts", "_offset", false, false),
                (true, "_ts", "_offset", true, false),
                (true, "_ts", "_offset", true, true),
                (false, "timestamp", "offset", false, false),
            ]
        {
            let mut sb = Schema::builder();
            let ts = if date { sb.add_date_field(ts_name, INDEXED | STORED | FAST) } else { sb.add_i64_field(ts_name, INDEXED | STORED | FAST) };
            let off = sb.add_i64_field(off_name, INDEXED | STORED | FAST);
            let index = new_index(sb.build(), sorted);
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            // Each commit must stay its own segment: a background merge finishing first
            // would leave one and the multi-segment case untested (CI caught it so).
            w.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
            // 8 commits = 8 segments; ms timestamps mostly rising with offset,
            // every 7th record from the past, several per second.
            let mut o = 0i64;
            for _ in 0..8 {
                for _ in 0..300 {
                    let ms = if burst { 1_000_000 + o * 7 - if o % 997 == 0 { 3_000 } else { 0 } } else if o % 7 == 0 { 1_000 + o * 3 } else { 1_000_000 + o * 300 };
                    let mut d = tantivy::TantivyDocument::new();
                    if date {
                        d.add_date(ts, tantivy::DateTime::from_timestamp_millis(ms));
                    } else {
                        d.add_i64(ts, ms);
                    }
                    d.add_i64(off, o);
                    w.add_document(d).unwrap();
                    o += 1;
                }
                w.commit().unwrap();
            }
            w.delete_term(tantivy::Term::from_field_i64(off, 2399));
            w.commit().unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();
            let fields = RankFields::of(&searcher).unwrap();
            for r in searcher.segment_readers() {
                assert_eq!(RankColumns::of(r, fields).newest_after.is_some(), sorted, "kept highest offset first");
            }
            assert!(searcher.segment_readers().len() > 1);
            // Same scores (1.0), but not an AllQuery: ranks every document.
            let every = tantivy::query::BooleanQuery::new(vec![(
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::AllQuery) as Box<dyn Query>,
            )]);
            // Independent of both: every document's whole key.
            let reference = |limit: usize| -> Vec<DocAddress> {
                let (ts_name, off_name) = (ts_name.to_string(), off_name.to_string());
                let collector = TopDocs::with_limit(limit).tweak_score(move |segment: &SegmentReader| {
                    let ff = segment.fast_fields();
                    let of = ff.i64(&off_name).unwrap();
                    let second: Box<dyn Fn(DocId) -> i64> = if date {
                        let t = ff.date(&ts_name).unwrap();
                        Box::new(move |d| t.first(d).unwrap().into_timestamp_secs())
                    } else {
                        let t = ff.i64(&ts_name).unwrap();
                        Box::new(move |d| t.first(d).unwrap().div_euclid(1000))
                    };
                    move |doc: DocId, score: Score| RankKey {
                        score,
                        timestamp_s: second(doc),
                        neg_partition: 0,
                        offset: of.first(doc).unwrap(),
                    }
                });
                searcher.search(&tantivy::query::AllQuery, &collector).unwrap().into_iter().map(|(_, a)| a).collect()
            };
            for limit in [1, 10, 299, 300, 301, 2_000, 5_000] {
                let fast = top_docs(&searcher, &tantivy::query::AllQuery, limit).unwrap();
                let full = top_docs(&searcher, &every, limit).unwrap();
                let addrs = |v: &[(Score, DocAddress)]| v.iter().map(|(_, a)| *a).collect::<Vec<_>>();
                assert_eq!(addrs(&fast), reference(limit), "match_all date={date} sorted={sorted} burst={burst} limit={limit}");
                assert_eq!(addrs(&full), reference(limit), "ranked date={date} sorted={sorted} burst={burst} limit={limit}");
                assert!(fast.iter().all(|(s, _)| *s == 1.0));
            }
        }
    }

    /// In indexes kept highest offset first, reading stops only where nothing
    /// later can place: (a) the 1,500 highest offsets stamped 10 minutes in
    /// the past (a producer with a slow clock), so the newest seconds lie past
    /// whole blocks of older ones; (b) two segments of one second with
    /// interleaved offsets (an archive written by several indexing threads),
    /// so a segment read later still holds higher offsets than the worst
    /// held. Every answer equals ranking every document.
    #[test]
    fn newest_first_segments_stop_only_where_nothing_later_can_place() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED, TEXT};
        for case in ["slow clock", "interleaved segments"] {
            let mut sb = Schema::builder();
            let ts = sb.add_date_field("_ts", INDEXED | STORED | FAST);
            let off = sb.add_i64_field("_offset", INDEXED | STORED | FAST);
            let body = sb.add_text_field("body", TEXT);
            let index = new_index(sb.build(), true);
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            // Each commit must stay its own segment: a background merge finishing first
            // would leave one and the multi-segment case untested (CI caught it so).
            w.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
            let doc = |o: i64, ms: i64| {
                let mut d = tantivy::TantivyDocument::new();
                d.add_date(ts, tantivy::DateTime::from_timestamp_millis(ms));
                d.add_i64(off, o);
                d.add_text(body, "timeout");
                d
            };
            match case {
                "slow clock" => {
                    const N: i64 = 6_000;
                    for o in 0..N {
                        let ms = 1_000_000_000 + o * 20 - if o >= N - 1_500 { 600_000 } else { 0 };
                        w.add_document(doc(o, ms)).unwrap();
                    }
                    w.commit().unwrap();
                }
                _ => {
                    for parity in [0, 1] {
                        for o in (parity..3_000).step_by(2) {
                            w.add_document(doc(o, 1_000_000_000 + (o / 1_000) * 1_000)).unwrap();
                        }
                        w.commit().unwrap();
                    }
                }
            }
            let searcher = index.reader().unwrap().searcher();
            let fields = RankFields::of(&searcher).unwrap();
            assert!(searcher.segment_readers().iter().all(|r| RankColumns::of(r, fields).newest_after.is_some()));
            let reference = |query: &dyn Query, limit: usize| -> Vec<DocAddress> {
                let collector = TopDocs::with_limit(limit).tweak_score(move |segment: &SegmentReader| {
                    let ff = segment.fast_fields();
                    let (t, of) = (ff.date("_ts").unwrap(), ff.i64("_offset").unwrap());
                    move |doc: DocId, score: Score| RankKey {
                        score,
                        timestamp_s: t.first(doc).unwrap().into_timestamp_secs(),
                        neg_partition: 0,
                        offset: of.first(doc).unwrap(),
                    }
                });
                searcher.search(query, &collector).unwrap().into_iter().map(|(_, a)| a).collect()
            };
            let term = tantivy::query::TermQuery::new(
                tantivy::Term::from_field_text(body, "timeout"),
                tantivy::schema::IndexRecordOption::WithFreqs,
            );
            let addrs = |v: Vec<(Score, DocAddress)>| v.into_iter().map(|(_, a)| a).collect::<Vec<_>>();
            for limit in [1, 10, 100, 1_000, 2_000] {
                let all = addrs(top_docs(&searcher, &tantivy::query::AllQuery, limit).unwrap());
                assert_eq!(all, reference(&tantivy::query::AllQuery, limit), "{case}: match_all limit={limit}");
                let scored = addrs(top_docs(&searcher, &term, limit).unwrap());
                assert_eq!(scored, reference(&term, limit), "{case}: term limit={limit}");
            }
        }
    }

    /// The score-gated collector returns exactly what building every
    /// document's whole key returns (`TopDocs::tweak_score`, what it replaced):
    /// varied BM25 scores, many equal ones, timestamps out of order and shared
    /// seconds, several segments, a deletion.
    #[test]
    fn score_gated_ranking_matches_ranking_every_document() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED, TEXT};
        let mut sb = Schema::builder();
        let ts = sb.add_date_field("_ts", INDEXED | STORED | FAST);
        let off = sb.add_i64_field("_offset", INDEXED | STORED | FAST);
        let body = sb.add_text_field("body", TEXT);
        let tag = sb.add_text_field("tag", tantivy::schema::STRING);
        // Like the archives' `_json`: no fieldnorms, so no stored block maxima
        // (Tantivy upstream returns wrong top hits when pruning these).
        let json = sb.add_json_field(
            "j",
            tantivy::schema::JsonObjectOptions::default().set_indexing_options(
                tantivy::schema::TextFieldIndexing::default()
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
            ),
        );
        let schema = sb.build();
        // In offset order, and newest second first as archives are written.
        for (sorted, burst) in [(false, false), (true, false), (true, true)] {
            let index = new_index(schema.clone(), sorted);
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            // Each commit must stay its own segment: a background merge finishing first
            // would leave one and the multi-segment case untested (CI caught it so).
            w.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
            let mut o = 0i64;
            for _ in 0..6 {
                for _ in 0..3_000 {
                    // "alpha" 0-3 times in documents of 3 lengths: scores repeat a lot.
                    let text = format!("{} {}", "alpha ".repeat((o % 4) as usize), "filler ".repeat((o % 3) as usize * 4));
                    let ms = if burst { 1_000_000 + o * 7 - if o % 997 == 0 { 3_000 } else { 0 } } else if o % 5 == 0 { 50_000 - o } else { 1_000_000 + o * 250 };
                    let mut d = tantivy::TantivyDocument::new();
                    d.add_date(ts, tantivy::DateTime::from_timestamp_millis(ms));
                    d.add_i64(off, o);
                    d.add_text(body, &text);
                    d.add_text(tag, ["red", "green", "blue"][(o % 7 % 3) as usize]);
                    let mut obj = std::collections::BTreeMap::new();
                    obj.insert("msg".to_string(), tantivy::schema::OwnedValue::Str(text.clone()));
                    obj.insert("level".to_string(), tantivy::schema::OwnedValue::Str(["error", "info"][(o % 11 % 2) as usize].into()));
                    d.add_object(json, obj);
                    w.add_document(d).unwrap();
                    o += 1;
                }
                w.commit().unwrap();
            }
            w.delete_term(tantivy::Term::from_field_i64(off, 1203));
            w.delete_term(tantivy::Term::from_field_i64(off, 9_001));
            w.commit().unwrap();
            let searcher = index.reader().unwrap().searcher();
            let fields = RankFields::of(&searcher).unwrap();
            for r in searcher.segment_readers() {
                assert_eq!(RankColumns::of(r, fields).newest_after.is_some(), sorted, "kept highest offset first");
            }
            assert!(searcher.segment_readers().len() > 1);

            let reference = |query: &dyn Query, limit: usize| -> Vec<DocAddress> {
                let collector = TopDocs::with_limit(limit).tweak_score(move |segment: &SegmentReader| {
                    let ff = segment.fast_fields();
                    let t = ff.date("_ts").unwrap();
                    let of = ff.i64("_offset").unwrap();
                    move |doc: DocId, score: Score| RankKey {
                        score,
                        timestamp_s: t.first(doc).unwrap().into_timestamp_secs(),
                        neg_partition: 0,
                        offset: of.first(doc).unwrap(),
                    }
                });
                searcher.search(query, &collector).unwrap().into_iter().map(|(_, a)| a).collect()
            };
            let text = tantivy::query::TermQuery::new(
                tantivy::Term::from_field_text(body, "alpha"),
                tantivy::schema::IndexRecordOption::WithFreqs,
            );
            let equal_scores = tantivy::query::ConstScoreQuery::new(Box::new(text.clone()), 1.0);
            let term = |f, w: &str| -> Box<dyn Query> {
                Box::new(tantivy::query::TermQuery::new(
                    tantivy::Term::from_field_text(f, w),
                    tantivy::schema::IndexRecordOption::WithFreqs,
                ))
            };
            use tantivy::query::{BooleanQuery, Occur};
            let union = BooleanQuery::new(vec![(Occur::Should, term(body, "alpha")), (Occur::Should, term(body, "filler"))]);
            let filtered = BooleanQuery::new(vec![(Occur::Must, term(body, "alpha")), (Occur::Must, term(tag, "red"))]);
            let must_filter = BooleanQuery::new(vec![
                (Occur::Must, Box::new(union.clone()) as Box<dyn Query>),
                (Occur::Must, Box::new(tantivy::query::ConstScoreQuery::new(term(tag, "green"), 0.0))),
            ]);
            let parser = tantivy::query::QueryParser::for_index(&index, vec![]);
            let json_term = parser.parse_query("j.msg:alpha").unwrap();
            let json_union = parser.parse_query("j.msg:alpha OR j.msg:filler").unwrap();
            let json_and = parser.parse_query("+j.msg:alpha +j.level:error").unwrap();
            let json_filtered = BooleanQuery::new(vec![(Occur::Must, term(body, "alpha")), (Occur::Must, parser.parse_query("j.level:error").unwrap())]);
            // A bool `filter` (crate::filtered_query): text scored, JSON
            // filtered; and a JSON match filtered by a term.
            let filter_json = crate::filtered_query::FilteredQuery::new(term(body, "alpha"), vec![parser.parse_query("j.level:error").unwrap()]);
            let filter_on_json = crate::filtered_query::FilteredQuery::new(parser.parse_query("j.msg:alpha").unwrap(), vec![term(tag, "red")]);
            let queries: [(&str, &dyn Query); 11] = [
                ("json_term", &*json_term),
                ("json_union", &*json_union),
                ("json_and", &*json_and),
                ("json_filtered", &json_filtered),
                ("filter_json", &filter_json),
                ("filter_on_json", &filter_on_json),
                ("bm25", &text),
                ("constant", &equal_scores),
                ("union", &union),
                ("filtered", &filtered),
                ("must_filter", &must_filter),
            ];
            for limit in [1, 7, 100, 399, 1_000, 5_000, 20_000] {
                for (name, query) in queries {
                    let got: Vec<DocAddress> = top_docs(&searcher, query, limit).unwrap().into_iter().map(|(_, a)| a).collect();
                    assert_eq!(got, reference(query, limit), "{name} sorted={sorted} burst={burst} limit={limit}");
                }
            }

            // A floor (another source's `limit`-th best) leaves out exactly the
            // documents strictly below it, whether it is passed per document or
            // lets whole segments and postings blocks be skipped.
            for (name, query) in queries {
                let all = ranked_docs(&searcher, query, 5_000, None, None).unwrap().unwrap();
                let newest = all.iter().map(|(k, _)| k.timestamp_s).max().unwrap();
                let mut floors: Vec<Floor> = [0usize, 3, 50, 398, 399, 1_000]
                    .iter()
                    .filter_map(|&i| all.get(i))
                    .flat_map(|(k, _)| [(k.score, k.timestamp_s), (k.score, k.timestamp_s + 1), (k.score.next_down(), k.timestamp_s)])
                    .collect();
                floors.push((all[0].0.score, newest + 1)); // only a higher score places
                floors.push((0.0, i64::MIN));
                for floor in floors {
                    for limit in [1, 7, 100, 1_000] {
                        let expected: Vec<DocAddress> = all
                            .iter()
                            .take(limit)
                            .filter(|(k, _)| !below_floor(Some(floor), k.score, || k.timestamp_s).0)
                            .map(|(_, a)| *a)
                            .collect();
                        let got: Vec<DocAddress> = ranked_docs(&searcher, query, limit, Some(floor), None)
                            .unwrap()
                            .unwrap()
                            .into_iter()
                            .map(|(_, a)| a)
                            .collect();
                        assert_eq!(got, expected, "{name} sorted={sorted} burst={burst} limit={limit} floor={floor:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn archived_rank_columns_are_reused_until_their_archive_closes() {
        use tantivy::schema::{Schema, FAST, INDEXED, STORED};
        for (ts_name, cached) in [("_ts", true), ("timestamp", false)] {
            let mut sb = Schema::builder();
            let ts = if cached { sb.add_date_field(ts_name, INDEXED | STORED | FAST) } else { sb.add_i64_field(ts_name, INDEXED | STORED | FAST) };
            let off = sb.add_i64_field(if cached { "_offset" } else { "offset" }, INDEXED | STORED | FAST);
            let index = tantivy::Index::create_in_ram(sb.build());
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            let mut d = tantivy::TantivyDocument::new();
            if cached { d.add_date(ts, tantivy::DateTime::from_timestamp_millis(5_000)) } else { d.add_i64(ts, 5_000) }
            d.add_i64(off, 1);
            w.add_document(d).unwrap();
            w.commit().unwrap();
            let searcher = index.reader().unwrap().searcher();
            let segment = &searcher.segment_readers()[0];
            let fields = RankFields::detect(searcher.schema()).unwrap();
            let first = RankColumns::of(segment, fields);
            assert_eq!(first.second(0), 5);
            assert_eq!(std::sync::Arc::ptr_eq(&first, &RankColumns::of(segment, fields)), cached, "{ts_name}");
            forget_segments([segment.segment_id()]);
            assert!(!std::sync::Arc::ptr_eq(&first, &RankColumns::of(segment, fields)), "{ts_name}: reopened after forget");
        }
    }

    fn hit(index: &str, score: f32, ts: i64, p: i64, off: i64) -> Hit {
        Hit {
            _index: index.to_string(),
            _id: format!("{p}-{off}"),
            _score: Some(score),
            _source: serde_json::json!({"timestamp": ts * 1000, "partition": p, "offset": off}),
            highlight: None,
            sort: None,
        }
    }

    #[test]
    fn score_then_newest_then_position() {
        let mut v = vec![
            hit("t", 1.0, 100, 0, 1),
            hit("t", 2.0, 50, 0, 2),
            hit("t", 1.0, 200, 1, 9),
            hit("t", 1.0, 100, 0, 3),
            hit("a", 1.0, 100, 2, 0),
        ];
        v.sort_by(hit_order);
        let ids: Vec<_> = v.iter().map(|h| format!("{}/{}", h._index, h._id)).collect();
        assert_eq!(ids, ["t/0-2", "t/1-9", "a/2-0", "t/0-3", "t/0-1"]);
    }

    #[test]
    fn pages_tile_the_list() {
        let all: Vec<Hit> = (0..25).map(|i| hit("t", 1.0, 1000 + (i % 7), i % 3, i)).collect();
        let (whole, n) = page(all.clone(), 0, 100, None);
        assert_eq!(n, 25);
        let mut tiled = Vec::new();
        for from in (0..25).step_by(10) {
            tiled.extend(page(all.clone(), from, 10, None).0);
        }
        let ids = |v: &[Hit]| v.iter().map(|h| h._id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&tiled), ids(&whole));
    }

    #[test]
    fn window_cap() {
        assert!(check_result_window(9_990, 10).is_ok());
        assert!(check_result_window(9_991, 10).unwrap_err().contains("[10001]"));
        assert!(check_result_window(usize::MAX, 10).is_err());
    }
}
