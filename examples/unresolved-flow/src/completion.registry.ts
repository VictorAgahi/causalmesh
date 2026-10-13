export function getCompletionConfig(name: string): { stream: string; type: string } {
  const registry: Record<string, { stream: string; type: string }> = {
    'user-deletion': { stream: ['user', 'deletion', 'success'].join(':'), type: 'user.deletion.success' },
  };
  return registry[name];
}
