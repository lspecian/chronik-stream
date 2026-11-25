// k6 query mix for the search benchmark against Chronik's /{topic}/_search.
// The same requests as the OpenSearch comparison (tests/os_adaptor/bench on
// the adaptor branch) and plain_tantivy_bench, so numbers line up.
// Each iteration runs every query once; latency is reported per query.
import http from 'k6/http';
import { check } from 'k6';

const BASE = __ENV.BASE || 'http://localhost:6092';
const TOPIC = __ENV.TOPIC || 'logs';
const T0 = 1767225600000;

const QUERIES = {
  match_text: { query: { match: { message: 'timeout' } }, size: 10 },
  term_level: { query: { term: { level: 'ERROR' } }, size: 10 },
  bool_text_filter: {
    query: { bool: { must: [{ match: { message: 'connection' } }], filter: [{ term: { service: 'svc-3' } }] } },
    size: 10,
  },
  match_all: { query: { match_all: {} }, size: 10 },
};
// Measured only when AGGS=1 (once aggregations are answered).
const AGG_QUERIES = {
  range_terms_agg: {
    size: 0,
    query: { range: { ts: { gte: T0 + 10 * 60 * 1000, lte: T0 + 20 * 60 * 1000 } } },
    aggs: { by_service: { terms: { field: 'service', size: 10 } } },
  },
  date_histogram: {
    size: 0,
    query: { match_all: {} },
    aggs: { per_minute: { date_histogram: { field: 'ts', fixed_interval: '1m' } } },
  },
};
const EVERY = __ENV.AGGS === '1' ? Object.assign({}, QUERIES, AGG_QUERIES) : QUERIES;
// ONLY=name[,name]: just those queries (profiling one query type).
const ONLY = (__ENV.ONLY || '').split(',').filter((s) => s);
const ALL = ONLY.length ? Object.fromEntries(Object.entries(Object.assign({}, QUERIES, AGG_QUERIES)).filter(([k]) => ONLY.includes(k))) : EVERY;

const thresholds = {};
for (const name of Object.keys(ALL)) thresholds[`http_req_duration{q:${name}}`] = ['p(99)<600000'];

export const options = {
  vus: Number(__ENV.VUS || 8),
  duration: __ENV.DURATION || '60s',
  thresholds,
  summaryTrendStats: ['avg', 'p(50)', 'p(95)', 'p(99)', 'max', 'count'],
};

export default function () {
  for (const [name, body] of Object.entries(ALL)) {
    const res = http.post(`${BASE}/${TOPIC}/_search`, JSON.stringify(body), {
      headers: { 'Content-Type': 'application/json' },
      tags: { q: name },
    });
    check(res, { [`${name} 200`]: (r) => r.status === 200 });
  }
}

export function handleSummary(data) {
  const out = {};
  for (const name of Object.keys(ALL)) {
    const m = data.metrics[`http_req_duration{q:${name}}`];
    if (m) out[name] = m.values;
  }
  out.iterations = data.metrics.iterations ? data.metrics.iterations.values : {};
  return { stdout: JSON.stringify(out, null, 1) + '\n' };
}
