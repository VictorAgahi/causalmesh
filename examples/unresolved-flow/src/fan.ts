import Redis from 'ioredis';

const redis = new Redis();

export async function emitFan() {
  await redis.xadd('fan:out', '*', 'k', 'v');
}

export async function fanConsumer0() {
  await redis.xreadgroup('GROUP', 'g0', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer1() {
  await redis.xreadgroup('GROUP', 'g1', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer2() {
  await redis.xreadgroup('GROUP', 'g2', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer3() {
  await redis.xreadgroup('GROUP', 'g3', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer4() {
  await redis.xreadgroup('GROUP', 'g4', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer5() {
  await redis.xreadgroup('GROUP', 'g5', 'w', 'STREAMS', 'fan:out', '>');
}

export async function fanConsumer6() {
  await redis.xreadgroup('GROUP', 'g6', 'w', 'STREAMS', 'fan:out', '>');
}

