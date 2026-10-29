export interface Subject {
  id: string;
}

export class Api {
  async listSubjects(): Promise<Subject[]> {
    return [];
  }
}

export const api = new Api();
