<script lang="ts">
  import TodoItem from './lib/TodoItem.svelte';
  import { addTodo } from './lib/store';

  function onAdd() {
    addTodo('write fixtures');
  }
</script>

<button on:click={onAdd}>Add</button>
<ul>
  <TodoItem label="first" />
</ul>
