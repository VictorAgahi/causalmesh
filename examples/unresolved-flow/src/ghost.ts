import Redis from 'ioredis';

const redis = new Redis();

export async function emitGhostEvents() {
  await redis.xadd('ghost:events', '*', 'k', 'v');
}

export async function listenGhostInbox() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'ghost:inbox', '>');
}
