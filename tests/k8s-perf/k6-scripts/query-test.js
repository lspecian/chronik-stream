import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend, Counter } from 'k6/metrics';

// Custom metrics
const queryErrors = new Rate('query_errors');
const queryLatency = new Trend('query_latency', true);
const queryCandidates = new Trend('query_candidates', true);
const queryResults = new Trend('query_results', true);
const queriesCompleted = new Counter('queries_completed');

const BASE_URL = __ENV.CHRONIK_URL || 'http://chronik-query.chronik-perf.svc.cluster.local:6092';
const TOPIC = __ENV.TOPIC || 'query-bench';

export const options = {
  stages: [
    { duration: '15s', target: 10 },    // warm up
    { duration: '30s', target: 50 },    // ramp to moderate
    { duration: '1m',  target: 200 },   // sustained high
    { duration: '1m',  target: 500 },   // peak load
    { duration: '30s', target: 200 },   // cool down
    { duration: '15s', target: 0 },     // ramp down
  ],
  thresholds: {
    http_req_duration: ['p(95)<2000'],  // 2s p95 for queries
    query_errors: ['rate<0.05'],        // 5% error budget
  },
};

// Realistic search queries across different categories
const queries = [
  // Single keyword
  'encryption', 'Kubernetes', 'pricing', 'timeout', 'PostgreSQL',
  'monitoring', 'deployment', 'firewall', 'API', 'backup',
  // Multi-word phrases
  'connection timeout error', 'auto scaling configuration',
  'load balancer setup', 'database connection pool',
  'TLS certificate renewal', 'budget alert threshold',
  'container deployment rollback', 'GPU instance types',
  'rate limit exceeded', 'VPC subnet configuration',
  // Technical questions
  'how to fix 502 error', 'configure auto scaling group',
  'set up monitoring dashboard', 'enable encryption at rest',
  'troubleshoot memory leak', 'optimize query performance',
  'Kubernetes pod scheduling', 'S3 object replication',
  'OAuth2 authentication flow', 'Terraform infrastructure code',
  // Edge cases
  'AES-256 FIPS 140-2 HSM', 'PyTorch TensorFlow training GPU',
  'SOC2 ISO-27001 HIPAA compliance', 'gRPC REST GraphQL API versioning',
];

const profiles = ['relevance', 'freshness', 'default'];
const formats = ['merged', 'grouped'];

export default function () {
  const headers = { 'Content-Type': 'application/json' };
  const query = queries[Math.floor(Math.random() * queries.length)];
  const profile = profiles[Math.floor(Math.random() * profiles.length)];
  const format = Math.random() < 0.8 ? 'merged' : 'grouped';
  const k = Math.random() < 0.7 ? 5 : (Math.random() < 0.5 ? 10 : 20);

  const payload = JSON.stringify({
    sources: [{ topic: TOPIC, modes: ['text'] }],
    q: { text: query },
    k: k,
    rank: { profile: profile },
    result_format: format,
  });

  const res = http.post(`${BASE_URL}/_query`, payload, { headers, timeout: '10s' });

  const ok = check(res, {
    'query status 200': (r) => r.status === 200,
    'has results or grouped_results': (r) => {
      if (r.status !== 200) return false;
      try {
        const body = JSON.parse(r.body);
        return (body.results && body.results.length >= 0) ||
               (body.grouped_results !== undefined);
      } catch (e) {
        return false;
      }
    },
    'has query_id': (r) => {
      if (r.status !== 200) return false;
      try {
        return JSON.parse(r.body).query_id !== undefined;
      } catch (e) {
        return false;
      }
    },
  });

  queryErrors.add(!ok);
  queryLatency.add(res.timings.duration);

  if (ok && res.status === 200) {
    try {
      const body = JSON.parse(res.body);
      queriesCompleted.add(1);
      if (body.stats) {
        queryCandidates.add(body.stats.candidates || 0);
      }
      if (body.results) {
        queryResults.add(body.results.length);
      }
    } catch (e) { /* ignore parse errors */ }
  }

  sleep(0.01); // 10ms between requests per VU
}
