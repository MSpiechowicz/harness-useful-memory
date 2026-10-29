<script lang="ts">
  import { countTodos } from './store';

  export let label: string;
  const position = countTodos();
</script>

<li>{position}. {label}</li>
