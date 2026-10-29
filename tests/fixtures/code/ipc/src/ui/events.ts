import { listen } from '@tauri-apps/api/event';

import { submitForm } from './form';

export function watchForm(): void {
  listen('form_opened', () => submitForm([]));
}
