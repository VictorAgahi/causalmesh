import Redis from 'ioredis';

const redis = new Redis();

export async function listenChain2() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'chain:2', '>');
  await redis.xadd('chain:3', '*', 'k', 'v');
}
