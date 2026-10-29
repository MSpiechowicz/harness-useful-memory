import { invoke } from '@tauri-apps/api/core';

export function validatePaths(paths: string[]): Promise<string[]> {
  return invoke('validate_paths', { paths });
}
