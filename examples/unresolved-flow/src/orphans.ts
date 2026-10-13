import Redis from 'ioredis';

const redis = new Redis();

// A literal consumer with no producer anywhere in the workspace.
export async function listenLegacyOrders() {
  await redis.xreadgroup('GROUP', 'legacy', 'worker-1', 'STREAMS', 'legacy:orders', '>');
}

// A literal producer with no consumer anywhere in the workspace.
export async function emitMetrics(value: string) {
  await redis.xadd('metrics:emitted', '*', 'value', value);
}
