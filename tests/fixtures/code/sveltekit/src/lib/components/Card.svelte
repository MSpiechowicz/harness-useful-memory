<script lang="ts">
  import { applyTheme } from '$lib/theme';

  export let title = '';
  const theme = applyTheme('dark');
</script>

<section class={theme}>{title}</section>
