import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain6() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:6', '>');
}
