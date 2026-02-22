import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend, Counter } from 'k6/metrics';

// Custom metrics
const produceErrors = new Rate('produce_errors');
const batchLatency = new Trend('batch_latency', true);
const messagesProduced = new Counter('messages_produced');

const BASE_URL = __ENV.INGESTOR_URL || 'http://ingestor.chronik-perf.svc.cluster.local:8080';

export const options = {
  stages: [
    { duration: '30s',  target: 200 },    // warm up fast
    { duration: '1m',   target: 1000 },   // ramp to 1K
    { duration: '2m',   target: 3000 },   // ramp to 3K
    { duration: '5m',   target: 5000 },   // sustained 5K — find the wall
    { duration: '3m',   target: 8000 },   // push to 8K — break it
    { duration: '2m',   target: 3000 },   // cool down
    { duration: '1m',   target: 0 },      // ramp down
  ],
  // No thresholds — pure stress test
  thresholds: {},
  noConnectionReuse: false,
};

// Pre-generate payload chars
const CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
function randomPayload(size) {
  let result = '';
  for (let i = 0; i < size; i++) {
    result += CHARS.charAt(Math.floor(Math.random() * CHARS.length));
  }
  return result;
}

export default function () {
  const headers = { 'Content-Type': 'application/json' };

  // All batch requests — 10 messages of 1KB each per request
  const batch = [];
  for (let i = 0; i < 10; i++) {
    batch.push({
      key: `k6-stress-${__VU}-${__ITER}-${i}`,
      value: randomPayload(1024),
    });
  }

  const res = http.post(`${BASE_URL}/produce/batch`, JSON.stringify(batch), {
    headers,
    timeout: '30s',
  });
  const ok = check(res, {
    'batch status 200': (r) => r.status === 200,
  });
  produceErrors.add(!ok);
  batchLatency.add(res.timings.duration);
  if (ok) messagesProduced.add(10);

  // No sleep — pure fire-hose
}
