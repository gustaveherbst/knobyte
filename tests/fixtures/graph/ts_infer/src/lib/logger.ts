export class Logger {
  info(message: string): void {
    console.log(message);
  }
  child(): Logger {
    return new Logger();
  }
}
