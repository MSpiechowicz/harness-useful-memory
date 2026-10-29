import { ref } from 'vue';

export function useCounter(start = 0) {
  const count = ref(start);

  function increment(): void {
    count.value += 1;
  }

  return { count, increment };
}
