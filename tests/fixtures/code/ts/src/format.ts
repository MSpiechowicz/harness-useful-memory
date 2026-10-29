import type { User } from './api';

export function formatName(user: User): string {
  return user.name.trim();
}
