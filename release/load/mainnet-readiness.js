import http from 'k6/http';
import { check, sleep } from 'k6';
import { Counter, Gauge, Rate, Trend } from 'k6/metrics';

const target = (__ENV.TARGET_URL || '').replace(/\/$/, '');
const token0 = __ENV.TOKEN_0 || 'TOKEN0';
const token1 = __ENV.TOKEN_1 || 'TOKEN1';
const paymentToken0 = __ENV.PAYMENT_TOKEN_0 || 'XLM';
const paymentToken1 = __ENV.PAYMENT_TOKEN_1 || 'USDC';
const expectedRate = Number(__ENV.EXPECTED_RATE || 100);
const maxRate = Number(__ENV.MAX_RATE || expectedRate * 5);
const duration = __ENV.TEST_DURATION || '10m';
const accounts = (__ENV.ACCOUNT_LIST || '').split(',').filter(Boolean);
const reconciliationUrl = __ENV.RECONCILIATION_URL || '';

const duplicateSubmissions = new Counter('duplicate_submissions');
const lostTerminalResults = new Counter('lost_terminal_results');
const queueDepth = new Gauge('queue_depth');
const queueDepthSamples = new Counter('queue_depth_samples');
const backpressure = new Rate('safe_backpressure');

export const options = {
  scenarios: {
    capacity: {
      executor: 'ramping-arrival-rate',
      startRate: Math.max(1, Math.floor(expectedRate / 4)),
      timeUnit: '1s',
      preAllocatedVUs: 50,
      maxVUs: 1000,
      stages: [
        { target: expectedRate, duration: '2m' },
        { target: expectedRate, duration },
        { target: maxRate, duration: '5m' },
      ],
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.01'],
    http_req_duration: ['p(95)<750', 'p(99)<1500'],
    duplicate_submissions: ['count==0'],
    lost_terminal_results: ['count==0'],
    safe_backpressure: ['rate<0.05'],
    queue_depth_samples: ['count>0'],
  },
};

function account(index) {
  if (index % 5 === 0) return __ENV.HOT_ACCOUNT;
  if (accounts.length === 0) throw new Error('ACCOUNT_LIST is required');
  return accounts[index % accounts.length];
}

function request(method, path, body, params = {}) {
  const response = http.request(method, `${target}${path}`, body, params);
  backpressure.add(response.status === 429 || response.status === 503);
  if (response.headers['X-Queue-Depth'] !== undefined) {
    queueDepth.add(Number(response.headers['X-Queue-Depth']));
    queueDepthSamples.add(1);
  }
  return response;
}

function quote(index) {
  const direction = index % 2 === 0 ? [token0, token1] : [token1, token0];
  const amount = index % 7 === 0 ? '9223372036854775807' : String(1000 + index);
  const response = request('GET', `/api/v1/pool/quote?amount_in=${amount}&token_in=${direction[0]}`);
  check(response, { 'quote succeeds': (r) => r.status === 200 });
}

function mixedRead(index) {
  const paths = ['/api/v1/pool/reserves', '/api/v1/pool/stats', `/api/v1/pool/lp-balance?address=${account(index)}`];
  const response = request('GET', paths[index % paths.length]);
  check(response, { 'pool read succeeds': (r) => r.status === 200 });
}

function prepareTransaction(index) {
  const stale = Math.floor(Date.now() / 1000) - 60;
  const future = Math.floor(Date.now() / 1000) + 300;
  const body = JSON.stringify({
    to: account(index),
    amount_0_out: index % 2 === 0 ? 0 : 1000,
    amount_1_out: index % 2 === 0 ? 1000 : 0,
    deadline: index % 11 === 0 ? stale : future,
  });
  const response = request('POST', '/api/v1/pool/build/swap', body, {
    headers: { 'Content-Type': 'application/json' },
    responseCallback: http.expectedStatuses(200, 400),
  });
  check(response, { 'valid preparation succeeds or stale is rejected': (r) => r.status === 200 || r.status === 400 });
}

function duplicateAndPoll(index) {
  const key = `load-${__VU}-${index}`;
  const body = JSON.stringify({
    sender: account(index), recipient: account(index + 1), amount: 10000000,
    token: index % 2 === 0 ? paymentToken0 : paymentToken1, idempotency_key: key,
  });
  const params = { headers: { 'Content-Type': 'application/json' } };
  const responses = http.batch([
    ['POST', `${target}/api/v1/payments`, body, params],
    ['POST', `${target}/api/v1/payments`, body, params],
  ]);
  const first = responses[0];
  const second = responses[1];
  backpressure.add(first.status === 429 || first.status === 503);
  backpressure.add(second.status === 429 || second.status === 503);
  if (first.status < 300 && second.status < 300 && first.body !== second.body) duplicateSubmissions.add(1);

  let id;
  try { id = first.json('id'); } catch (_) { return; }
  if (!id) return;
  let terminal = false;
  for (let attempt = 0; attempt < 10; attempt += 1) {
    const status = request('GET', `/api/v1/payments/${id}`);
    if (status.status === 200) {
      const value = status.json('status');
      if (value === 'confirmed' || value === 'failed') { terminal = true; break; }
    }
    sleep(0.25);
  }
  if (!terminal) lostTerminalResults.add(1);
  request('GET', '/api/v1/payments');
  if (!reconciliationUrl) throw new Error('RECONCILIATION_URL is required');
  const catchup = http.get(reconciliationUrl);
  check(catchup, {
    'event reconciliation catches up': (r) =>
      r.status === 200 && (r.json('caught_up') === true || r.json('lag') === 0),
  });
}

export default function () {
  if (!target) throw new Error('TARGET_URL is required');
  const selector = (__ITER + __VU) % 10;
  if (selector < 4) quote(__ITER);
  else if (selector < 7) mixedRead(__ITER);
  else if (selector < 9) prepareTransaction(__ITER);
  else duplicateAndPoll(__ITER);
}

export function handleSummary(data) {
  return { 'artifacts/k6-summary.json': JSON.stringify(data, null, 2) };
}
