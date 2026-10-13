import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain5() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:5', '>');
  await redis.xadd('chain:6', '*', 'k', 'v');
}
