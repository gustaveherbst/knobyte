export interface Entity {
  id: string;
  describe(): string;
}

export class User implements Entity {
  constructor(public id: string, private name: string) {}
  describe(): string {
    return this.name;
  }
  rename(name: string): this {
    this.name = name;
    return this;
  }
}

export type UserAlias = User;
