import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain0() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:0', '>');
  await redis.xadd('chain:1', '*', 'k', 'v');
}
