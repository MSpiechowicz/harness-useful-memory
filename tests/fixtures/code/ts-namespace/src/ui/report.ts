import * as math from '../lib/math';
import * as lib from '../lib';

export function report(n: number) {
  const made = new math.Counter();
  return [math.double(n), math.triple(n), made];
}

export function viaBarrel(n: number) {
  return lib.triple(n);
}
