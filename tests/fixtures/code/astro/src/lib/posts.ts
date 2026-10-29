export interface Post {
  slug: string;
}

export function getPosts(): Post[] {
  return [{ slug: 'hello' }];
}
