import { loadUserName } from '@/user';

export async function main(): Promise<void> {
  console.log(await loadUserName(1));
}
