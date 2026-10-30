export interface Notifier {
  send(message: string): void;
}

export class EmailNotifier implements Notifier {
  send(message: string): void {
    deliverMail(message);
  }
}

export class SmsNotifier implements Notifier {
  send(message: string): void {
    deliverText(message);
  }
}

export class Base {
  run(): void {}
}

export class Loud extends Base {
  run(): void {
    shout();
  }
}

function deliverMail(message: string): void {}
function deliverText(message: string): void {}
function shout(): void {}
