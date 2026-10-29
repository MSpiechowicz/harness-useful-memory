<script setup lang="ts">
import { useCounter } from './composables/useCounter';

const { count, increment } = useCounter(1);
</script>

<template>
  <button @click="increment">{{ count }}</button>
</template>
