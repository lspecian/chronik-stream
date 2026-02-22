import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend, Counter } from 'k6/metrics';

// Custom metrics
const produceErrors = new Rate('produce_errors');
const produceLatency = new Trend('produce_latency', true);
const batchLatency = new Trend('batch_latency', true);
const messagesProduced = new Counter('messages_produced');

const BASE_URL = __ENV.INGESTOR_URL || 'http://ingestor.chronik-perf.svc.cluster.local:8080';

export const options = {
  stages: [
    { duration: '30s', target: 50 },    // ramp up
    { duration: '1m',  target: 200 },   // sustained medium
    { duration: '1m',  target: 500 },   // sustained high
    { duration: '1m',  target: 200 },   // cool down
    { duration: '30s', target: 0 },     // ramp down
  ],
  thresholds: {
    http_req_duration: ['p(95)<200'],
    produce_errors: ['rate<0.01'],
  },
};

// Generate random payload of given size
function randomPayload(size) {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  let result = '';
  for (let i = 0; i < size; i++) {
    result += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return result;
}

export default function () {
  const headers = { 'Content-Type': 'application/json' };
  const iteration = __ITER;

  if (iteration % 3 === 0) {
    // Single produce (2 out of 3 iterations)
    const payload = JSON.stringify({
      key: `k6-key-${__VU}-${iteration}`,
      value: randomPayload(1024),
    });

    const res = http.post(`${BASE_URL}/produce`, payload, { headers });
    const ok = check(res, {
      'produce status 200': (r) => r.status === 200,
    });
    produceErrors.add(!ok);
    produceLatency.add(res.timings.duration);
    if (ok) messagesProduced.add(1);
  } else {
    // Batch produce (1 out of 3 iterations)
    const batch = [];
    for (let i = 0; i < 10; i++) {
      batch.push({
        key: `k6-batch-${__VU}-${iteration}-${i}`,
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
  }

  sleep(0.01); // 10ms between requests per VU
}
