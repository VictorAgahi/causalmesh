import Redis from 'ioredis';

const redis = new Redis();

export async function listenGhostEventsInTest() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'ghost:events', '>');
}

export async function emitGhostInboxInTest() {
  await redis.xadd('ghost:inbox', '*', 'k', 'v');
}

export async function emitGhostOnly() {
  await redis.xadd('ghost:only', '*', 'k', 'v');
}

export async function listenGhostOnly() {
  await redis.xreadgroup('GROUP', 'g', 'w', 'STREAMS', 'ghost:only', '>');
}
