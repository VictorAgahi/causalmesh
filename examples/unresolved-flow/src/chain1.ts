import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain1() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:1', '>');
  await redis.xadd('chain:2', '*', 'k', 'v');
}
