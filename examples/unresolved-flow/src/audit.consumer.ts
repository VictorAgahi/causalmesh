import Redis from 'ioredis';

const redis = new Redis();

export async function listenCachePurged() {
  await redis.xreadgroup('GROUP', 'audit', 'worker-1', 'STREAMS', 'cache:purged', '>');
}
