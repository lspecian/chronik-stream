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
    { duration: '30s',  target: 100 },   // warm up
    { duration: '1m',   target: 500 },   // ramp
    { duration: '2m',   target: 2000 },  // heavy load
    { duration: '3m',   target: 5000 },  // max load — find the ceiling
    { duration: '2m',   target: 2000 },  // cool down
    { duration: '1m',   target: 0 },     // ramp down
  ],
  // No latency thresholds — we want to find the breaking point
  thresholds: {
    produce_errors: ['rate<0.05'],  // allow up to 5% errors under extreme load
  },
};

// Pre-generate a random payload template (reuse to reduce CPU in k6)
const PAYLOAD_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
function randomPayload(size) {
  let result = '';
  for (let i = 0; i < size; i++) {
    result += PAYLOAD_CHARS.charAt(Math.floor(Math.random() * PAYLOAD_CHARS.length));
  }
  return result;
}

export default function () {
  const headers = { 'Content-Type': 'application/json' };

  // All batch requests for maximum throughput — 10 messages per request
  const batch = [];
  for (let i = 0; i < 10; i++) {
    batch.push({
      key: `k6-max-${__VU}-${__ITER}-${i}`,
      value: randomPayload(1024),
    });
  }

  const res = http.post(`${BASE_URL}/produce/batch`, JSON.stringify(batch), { headers });
  const ok = check(res, {
    'batch status 200': (r) => r.status === 200,
  });
  produceErrors.add(!ok);
  batchLatency.add(res.timings.duration);
  if (ok) messagesProduced.add(10);

  // No sleep — pure fire-hose mode
}
