import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend, Counter } from 'k6/metrics';

// ─── Metrics ─────────────────────────────────────────────────────────────────

const textErrors = new Rate('text_errors');
const textLatency = new Trend('text_latency', true);
const textCompleted = new Counter('text_completed');

const sqlErrors = new Rate('sql_errors');
const sqlLatency = new Trend('sql_latency', true);
const sqlCompleted = new Counter('sql_completed');

const totalErrors = new Rate('total_errors');
const totalLatency = new Trend('total_latency', true);
const totalCompleted = new Counter('total_completed');

// ─── Configuration ───────────────────────────────────────────────────────────

const BASE_URL = __ENV.CHRONIK_URL || 'http://chronik-thunderbird.chronik-perf.svc.cluster.local:6092';
const TOPIC = __ENV.TOPIC || 'thunderbird';

// Test mode: text, sql, mixed (default)
const TEST_MODE = __ENV.TEST_MODE || 'mixed';

// ─── VU Stages ───────────────────────────────────────────────────────────────

export const options = (() => {
  switch (TEST_MODE) {
    case 'text':
      return {
        stages: [
          { duration: '15s', target: 100 },
          { duration: '30s', target: 500 },
          { duration: '1m',  target: 1000 },
          { duration: '1m',  target: 2000 },
          { duration: '1m',  target: 2000 },  // sustained peak
          { duration: '30s', target: 500 },
          { duration: '15s', target: 0 },
        ],
        thresholds: {
          text_errors: ['rate<0.05'],
          text_latency: ['p(50)<10', 'p(95)<50', 'p(99)<200'],
        },
      };
    case 'sql':
      return {
        stages: [
          { duration: '15s', target: 50 },
          { duration: '30s', target: 200 },
          { duration: '1m',  target: 500 },
          { duration: '1m',  target: 500 },  // sustained peak
          { duration: '30s', target: 200 },
          { duration: '15s', target: 0 },
        ],
        thresholds: {
          sql_errors: ['rate<0.05'],
          sql_latency: ['p(50)<100', 'p(95)<500'],
        },
      };
    default: // mixed
      return {
        stages: [
          { duration: '15s', target: 50 },
          { duration: '30s', target: 200 },
          { duration: '1m',  target: 500 },
          { duration: '1m',  target: 1000 },
          { duration: '1m',  target: 1000 },  // sustained peak
          { duration: '30s', target: 500 },
          { duration: '15s', target: 0 },
        ],
        thresholds: {
          total_errors: ['rate<0.05'],
        },
      };
  }
})();

// ─── Query Pools ─────────────────────────────────────────────────────────────

// Text search queries — based on real Thunderbird log content
const textQueries = [
  // Simple single-term (high frequency in logs)
  'error', 'warning', 'failure', 'timeout', 'kernel',
  // Multi-term (component + keyword)
  'network interface down', 'disk I/O error', 'memory allocation failed',
  'postfix warning', 'tftp client', 'kernel panic',
  // Component-specific
  'sshd', 'postdrop', 'ntpd', 'rsync', 'gmetad',
  // Hostname patterns
  'tbird-admin', 'aadmin1',
  // Anomaly labels (ECC/CPU events)
  'ECC memory', 'CPU ticks', 'DIMM',
  // System messages
  'synchronized to', 'topology change', 'startup succeeded',
  'publickey', 'No such file or directory',
];

// SQL queries — validate DataFusion at 16.6M rows
// Note: Hot buffer stores raw Kafka columns (_topic, _partition, _offset, _timestamp, _key, _value)
// Structured field extraction (hostname, component) requires JSON schema configuration
const sqlQueries = [
  // COUNT — core hot buffer performance
  `SELECT COUNT(*) FROM ${TOPIC}_hot`,

  // GROUP BY partition — data distribution across partitions
  `SELECT _partition, COUNT(*) as cnt FROM ${TOPIC}_hot GROUP BY _partition ORDER BY cnt DESC`,

  // SELECT with offset range
  `SELECT _offset, _key, _timestamp FROM ${TOPIC}_hot WHERE _offset > 1000 AND _offset < 2000 LIMIT 50`,

  // SELECT recent by timestamp
  `SELECT _offset, _key FROM ${TOPIC}_hot ORDER BY _timestamp DESC LIMIT 20`,

  // COUNT on cold table (Parquet)
  `SELECT COUNT(*) FROM ${TOPIC}_cold`,

  // COUNT on unified view (hot + cold)
  `SELECT COUNT(*) FROM ${TOPIC}`,

  // GROUP BY partition on cold
  `SELECT _partition, COUNT(*) as cnt FROM ${TOPIC}_cold GROUP BY _partition ORDER BY cnt DESC`,

  // Offset range on cold
  `SELECT _offset, _key, _timestamp FROM ${TOPIC}_cold WHERE _offset > 5000 AND _offset < 6000 LIMIT 50`,
];

const headers = { 'Content-Type': 'application/json' };

// ─── Test Functions ──────────────────────────────────────────────────────────

function textSearch() {
  const q = textQueries[Math.floor(Math.random() * textQueries.length)];
  const k = Math.random() < 0.7 ? 10 : (Math.random() < 0.5 ? 20 : 50);
  const profile = Math.random() < 0.7 ? 'relevance' : 'freshness';

  const payload = JSON.stringify({
    sources: [{ topic: TOPIC, modes: ['text'] }],
    q: { text: q },
    k: k,
    rank: { profile: profile },
    result_format: 'merged',
  });

  const res = http.post(`${BASE_URL}/_query`, payload, { headers, timeout: '10s' });
  const ok = check(res, { 'text 200': (r) => r.status === 200 });
  textErrors.add(!ok);
  textLatency.add(res.timings.duration);
  if (ok) textCompleted.add(1);
  totalErrors.add(!ok);
  totalLatency.add(res.timings.duration);
  if (ok) totalCompleted.add(1);
}

function sqlQuery() {
  const q = sqlQueries[Math.floor(Math.random() * sqlQueries.length)];

  const payload = JSON.stringify({ query: q });
  const res = http.post(`${BASE_URL}/_sql`, payload, { headers, timeout: '30s' });
  const ok = check(res, { 'sql 200': (r) => r.status === 200 });
  sqlErrors.add(!ok);
  sqlLatency.add(res.timings.duration);
  if (ok) sqlCompleted.add(1);
  totalErrors.add(!ok);
  totalLatency.add(res.timings.duration);
  if (ok) totalCompleted.add(1);
}

// ─── Main ────────────────────────────────────────────────────────────────────

export default function () {
  switch (TEST_MODE) {
    case 'text':
      textSearch();
      break;
    case 'sql':
      sqlQuery();
      break;
    default: // mixed: 60% text, 40% SQL
      if (Math.random() < 0.6) {
        textSearch();
      } else {
        sqlQuery();
      }
      break;
  }
  sleep(0.01);
}
