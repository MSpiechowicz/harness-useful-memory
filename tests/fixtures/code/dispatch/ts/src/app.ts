import { Base, Notifier } from "./notify";

export function alert(notifier: Notifier): void {
  notifier.send("disk full");
}

export function start(base: Base): void {
  base.run();
}
