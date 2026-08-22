import { Clock } from "./util/clock";

export class Legacy {
  constructor() {
    this.clock = new Clock();
  }
  tick() {
    return this.clock.now();
  }
}
