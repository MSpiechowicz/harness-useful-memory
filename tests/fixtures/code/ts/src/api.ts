export interface User {
  id: number;
  name: string;
}

export async function fetchUser(id: number): Promise<User> {
  const response = await fetch(`/users/${id}`);
  return response.json();
}

export class Client {
  get(url: string): Promise<Response> {
    return fetch(url);
  }
}
