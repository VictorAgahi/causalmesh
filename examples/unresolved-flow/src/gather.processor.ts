import Redis from 'ioredis';
import { getCompletionConfig } from './completion.registry';

const redis = new Redis();

export async function listenDeletionRequests() {
  await redis.xreadgroup('GROUP', 'gather', 'worker-1', 'STREAMS', 'user:deletion:requested', '>');
}

export class GatherPostProcessor {
  async complete(userId: string) {
    const cfg = getCompletionConfig('user-deletion');
    await redis.xadd(cfg.stream, '*', 'type', cfg.type, 'userId', userId);
  }
}
