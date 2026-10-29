import type { Subject } from './api';

// Same method name as Api, a different class: only a receiver type tells them apart.
export class MockApi {
  async listSubjects(): Promise<Subject[]> {
    return [{ id: 'mock' }];
  }
}
