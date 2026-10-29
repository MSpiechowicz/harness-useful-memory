import { validatePaths } from '../lib/api';

export async function submitForm(paths: string[]): Promise<boolean> {
  const valid = await validatePaths(paths);
  return valid.length === paths.length;
}
