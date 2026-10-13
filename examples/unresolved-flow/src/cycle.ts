import Redis from 'ioredis';

const redis = new Redis();

export async function listenCycleA() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'cycle:a', '>');
  await redis.xadd('cycle:b', '*', 'k', 'v');
}

export async function listenCycleB() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'cycle:b', '>');
  await redis.xadd('cycle:a', '*', 'k', 'v');
}
