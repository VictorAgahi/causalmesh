import Redis from 'ioredis';

const redis = new Redis();

export async function requestUserDeletion(userId: string) {
  await redis.xadd('user:deletion:requested', '*', 'type', 'user.deletion.requested', 'userId', userId);
}
