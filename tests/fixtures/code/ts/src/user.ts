import { fetchUser } from './api';
import { formatName } from './format';

export async function loadUserName(id: number): Promise<string> {
  const user = await fetchUser(id);
  return formatName(user);
}
