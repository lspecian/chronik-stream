# Tantivy fork

Chronik builds against a fork of Tantivy, not the crates.io release:
branch `chronik` of <https://github.com/lspecian/tantivy>, pinned by commit in
the `[patch.crates-io]` section of the workspace `Cargo.toml`. All nine
crates of the Tantivy workspace that Chronik's lock file holds (`tantivy`,
`tantivy-bitpacker`, `-columnar`, `-common`, `-query-grammar`, `-sstable`,
`-stacker`, `-tokenizer-api`, `ownedbytes`) come from the same commit; patching
only `tantivy` would link two copies of the others.

## Why

Upstream is active (about 30 merges a month in 2026), but outside correctness
fixes for features its maintainers do not lean on can wait weeks for a
review. The fork lets Chronik carry such fixes now and drop each one when
upstream releases it.

## Rules

- **Track upstream.** The branch is an upstream release tag plus a short
  list of patches. On each upstream release, rebase the patches onto the new
  tag, run Tantivy's tests and Chronik's search tests, push, and move the pin.
- **Keep the index format.** Archives are Tantivy indexes. A patch that
  changes what is written ties every archive to the fork and needs a full
  `POST /admin/reindex`; prefer fixes at query time. A format change needs
  its own decision.
- **Send fixes upstream.** Each patch links its upstream issue or PR. A
  patch upstream has released is dropped at the next rebase.
- **Not published.** A git dependency needs no renamed crates. Publishing to
  crates.io would mean renaming all nine crates and owning them publicly;
  nothing in Chronik needs it (no Chronik crate is published).

## Patches on 0.27.0

| Patch | Upstream | What it fixes |
|---|---|---|
| Block-WAND bounds without fieldnorms (15 commits, cherry-picked) | issue #3230, PR #3236 (open) | Fields without fieldnorms, JSON fields among them, store zero block maxima; Block-WAND then skipped blocks holding real hits (a 50K-document repro visited 8 of 5,000 matches). Missing bounds are now worked out at query time, so existing archives are fixed without a reindex. Also fixes a panic in conjunctions on JSON numeric postings. |

Chronik relies on it in `RankCollector::collect_segment`
(`crates/chronik-search/src/ranking.rs`), which prunes with
`Weight::for_each_pruning`. `score_gated_ranking_matches_ranking_every_document`
covers JSON-field queries and fails against upstream 0.27.0
(`json_union limit=1`).

Not carried: PR #3231 (conjunction pruning bounds, same author) rewrites the
same bound code as #3236 in a different way; the two do not combine without
redesigning it.

## Rebasing

```bash
cd ../tantivy                       # clone of the fork, remote `upstream` = quickwit-oss/tantivy
git fetch upstream --tags
git checkout -b chronik-<new> <new-tag>
git cherry-pick <patches still unreleased upstream>
cargo test --release --lib          # 2 should_panic tests in disjunction fail in release builds upstream too
git push origin chronik-<new>
# in chronik-stream: set rev = "<commit>" on all nine entries, then
cargo update -w && cargo test -p chronik-search --lib
```
