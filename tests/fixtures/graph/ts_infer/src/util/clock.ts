export class Clock {
  now(): number {
    return Date.now();
  }
  self(): Clock {
    return this;
  }
}
