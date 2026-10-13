import Redis from 'ioredis';

const redis = new Redis();

export async function listenDeletionSuccess() {
  await redis.xreadgroup('GROUP', 'purge', 'worker-1', 'STREAMS', 'user:deletion:success', '>');
}

export async function purgeCaches(userId: string) {
  await redis.xadd('cache:purged', '*', 'userId', userId);
}
