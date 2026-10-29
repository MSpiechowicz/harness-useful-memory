<script lang="ts">
  import Card from '$lib/components/Card.svelte';
  import { formatDate } from '$lib/format';

  const today = formatDate('2026-01-01T00:00:00Z');
</script>

<Card title={today} />
