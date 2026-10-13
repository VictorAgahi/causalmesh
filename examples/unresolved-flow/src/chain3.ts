import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain3() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:3', '>');
  await redis.xadd('chain:4', '*', 'k', 'v');
}
