import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain4() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:4', '>');
  await redis.xadd('chain:5', '*', 'k', 'v');
}
