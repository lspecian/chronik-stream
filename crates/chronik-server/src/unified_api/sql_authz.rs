//! Per-topic authorization for `/_sql` (Security Phase 4).
//!
//! Phase 4 authenticated the HTTP data plane: without `CHRONIK_API_KEY` nobody
//! gets in. It did not *authorize* — a key holder could `SELECT * FROM
//! any_topic`, so the ACLs enforced on the Kafka port did not apply here.
//!
//! # Why the table→topic mapping is the whole problem
//!
//! SQL table names are derived from topic names by replacing every
//! non-alphanumeric character with `_`. That is **lossy and not invertible**:
//!
//! ```text
//! mem.raw.orders  ─┐
//! mem-raw-orders  ─┼─►  mem_raw_orders
//! mem_raw_orders  ─┘
//! ```
//!
//! Reversing it by string surgery would be a silent security bug: authorize
//! `mem_raw_orders` and you might have checked the wrong topic entirely. So the
//! map is built in the **forward** direction from the live topic list, and where
//! several topics collapse onto one table name, **every** candidate must be
//! authorized. Ambiguity denies; it never admits.

use std::collections::{HashMap, HashSet};

/// The topics a SQL table name could refer to.
///
/// More than one when distinct topic names sanitize to the same table name.
#[derive(Debug, Clone, Default)]
pub struct TableTopicMap {
    /// sanitized table name -> every topic that produces it
    by_table: HashMap<String, Vec<String>>,
}

impl TableTopicMap {
    /// Build the map from the live topic list.
    pub fn from_topics<I, S>(topics: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut by_table: HashMap<String, Vec<String>> = HashMap::new();
        for topic in topics {
            let topic = topic.as_ref();
            by_table
                .entry(sanitize_table_name(topic))
                .or_default()
                .push(topic.to_string());
        }
        Self { by_table }
    }

    /// Topics a table reference could name.
    ///
    /// Strips the `_hot` / `_cold` suffixes the SQL layer appends to the
    /// per-tier views, so `orders_cold` resolves to the `orders` topic. Returns
    /// an empty slice for a name that matches no topic — a CTE, an
    /// `information_schema` table, or simply a typo. Those carry no topic data,
    /// so there is nothing to authorize; the query fails on its own if the table
    /// does not exist.
    pub fn topics_for(&self, table: &str) -> &[String] {
        let base = table
            .strip_suffix("_hot")
            .or_else(|| table.strip_suffix("_cold"))
            .unwrap_or(table);

        // Check the unstripped name first: a topic could legitimately be called
        // "orders_cold", and it must not be mistaken for the cold view of
        // "orders".
        if let Some(topics) = self.by_table.get(table) {
            return topics;
        }
        self.by_table.get(base).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Whether any topic maps to this table name.
    pub fn is_topic_table(&self, table: &str) -> bool {
        !self.topics_for(table).is_empty()
    }
}

/// Mirror of the SQL layer's topic→table sanitiser.
///
/// Kept in step with `SqlHandler::sanitize_table_name`; a test below pins them
/// together, because a divergence would silently authorize the wrong topic.
pub fn sanitize_table_name(topic: &str) -> String {
    topic
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the table names a SQL statement reads.
///
/// Uses DataFusion's own resolver rather than a regex, so CTEs, subqueries and
/// joins are handled the way the planner handles them — in particular a CTE
/// named `orders` shadows the table `orders` and must not be treated as a topic
/// reference.
///
/// Returns `Err` for SQL that cannot be parsed; the caller should refuse rather
/// than run it, since an unparseable statement cannot be authorized.
pub fn referenced_tables(sql: &str) -> Result<Vec<String>, String> {
    use datafusion::sql::parser::DFParser;

    let statements = DFParser::parse_sql(sql).map_err(|e| format!("{}", e))?;

    let mut tables = Vec::new();
    for statement in &statements {
        let (refs, ctes) = datafusion::catalog_common::resolve_table_references(statement, true)
            .map_err(|e| format!("{}", e))?;
        let cte_names: HashSet<String> = ctes.iter().map(|c| c.table().to_string()).collect();
        for reference in refs {
            let name = reference.table().to_string();
            // A CTE is not a topic. Authorizing it would deny a legitimate query
            // over a name that never touches stored data.
            if !cte_names.contains(&name) {
                tables.push(name);
            }
        }
    }
    Ok(tables)
}

/// The topics a statement needs read access to.
///
/// Every candidate of an ambiguous table name is included, so the caller must
/// hold Read on all of them.
pub fn required_topics(sql: &str, map: &TableTopicMap) -> Result<Vec<String>, String> {
    let mut topics = HashSet::new();
    for table in referenced_tables(sql)? {
        for topic in map.topics_for(&table) {
            topics.insert(topic.clone());
        }
    }
    let mut topics: Vec<String> = topics.into_iter().collect();
    topics.sort();
    Ok(topics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitiser_matches_the_sql_layer() {
        // Pinned against crate::unified_api::sql_handler::SqlHandler::sanitize_table_name.
        // If that changes and this does not, authorization silently targets the
        // wrong topic.
        for topic in [
            "orders",
            "mem.raw.orders",
            "mem-raw-orders",
            "a.b-c_d",
            "UPPER.Case",
        ] {
            assert_eq!(
                sanitize_table_name(topic),
                crate::unified_api::sql_handler::SqlHandler::sanitize_table_name(topic),
                "sanitiser diverged for {}",
                topic
            );
        }
    }

    #[test]
    fn resolves_a_simple_select() {
        let tables = referenced_tables("SELECT * FROM orders").unwrap();
        assert_eq!(tables, vec!["orders"]);
    }

    #[test]
    fn resolves_joins_and_subqueries() {
        let tables =
            referenced_tables("SELECT * FROM a JOIN b ON a.id = b.id WHERE a.x IN (SELECT x FROM c)")
                .unwrap();
        let set: HashSet<_> = tables.into_iter().collect();
        assert!(set.contains("a"));
        assert!(set.contains("b"));
        assert!(set.contains("c"), "a subquery's table must be authorized too");
    }

    /// A CTE shadows a table name and must not be treated as a topic - otherwise
    /// a legitimate query gets denied over a name that touches no stored data.
    #[test]
    fn a_cte_is_not_a_topic_reference() {
        let tables =
            referenced_tables("WITH orders AS (SELECT 1 AS x) SELECT * FROM orders").unwrap();
        assert!(
            !tables.contains(&"orders".to_string()),
            "the CTE was mistaken for a topic: {:?}",
            tables
        );
    }

    /// The important one: distinct topics collapsing onto one table name means
    /// EVERY candidate must be authorized.
    #[test]
    fn an_ambiguous_table_requires_every_candidate() {
        let map = TableTopicMap::from_topics(["mem.raw.orders", "mem-raw-orders", "other"]);
        let topics = required_topics("SELECT * FROM mem_raw_orders", &map).unwrap();
        assert_eq!(
            topics,
            vec!["mem-raw-orders".to_string(), "mem.raw.orders".to_string()],
            "an ambiguous table name must require authorization on all candidates, \
             or checking one of them authorizes reading the other"
        );
    }

    #[test]
    fn hot_and_cold_views_resolve_to_the_topic() {
        let map = TableTopicMap::from_topics(["orders"]);
        assert_eq!(map.topics_for("orders_hot"), ["orders".to_string()]);
        assert_eq!(map.topics_for("orders_cold"), ["orders".to_string()]);
        assert_eq!(map.topics_for("orders"), ["orders".to_string()]);
    }

    /// A topic genuinely named `x_cold` must not be confused with the cold view
    /// of a topic named `x`.
    #[test]
    fn a_topic_named_like_a_view_wins() {
        let map = TableTopicMap::from_topics(["orders_cold", "orders"]);
        assert_eq!(map.topics_for("orders_cold"), ["orders_cold".to_string()]);
    }

    /// An unknown table is not a topic, so there is nothing to authorize. The
    /// query fails on its own if the table does not exist.
    #[test]
    fn an_unknown_table_requires_nothing() {
        let map = TableTopicMap::from_topics(["orders"]);
        assert!(map.topics_for("information_schema_tables").is_empty());
        assert!(required_topics("SELECT 1", &map).unwrap().is_empty());
    }

    /// Unparseable SQL must be an error, not an empty requirement set - an empty
    /// set would mean "authorize nothing" and run the statement.
    #[test]
    fn unparseable_sql_is_an_error() {
        let map = TableTopicMap::from_topics(["orders"]);
        assert!(required_topics("SELECT FROM WHERE ((", &map).is_err());
    }

    #[test]
    fn topics_are_deduplicated_and_sorted() {
        let map = TableTopicMap::from_topics(["a", "b"]);
        let topics = required_topics("SELECT * FROM a JOIN b ON a.i=b.i JOIN a a2 ON a2.i=b.i", &map)
            .unwrap();
        assert_eq!(topics, vec!["a".to_string(), "b".to_string()]);
    }
}
