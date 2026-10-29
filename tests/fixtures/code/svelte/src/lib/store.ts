export interface Todo {
  text: string;
}

const todos: Todo[] = [];

export function addTodo(text: string): Todo {
  const todo = { text };
  todos.push(todo);
  return todo;
}

export function countTodos(): number {
  return todos.length;
}
