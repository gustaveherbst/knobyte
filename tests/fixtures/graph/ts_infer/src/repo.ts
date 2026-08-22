import { User } from "./models";

export class BaseRepo {
  protected log(msg: string): void {
    console.log(msg);
  }
  count(): number {
    return 0;
  }
}

export class UserRepo extends BaseRepo {
  private cache = new Map<string, User>();
  find(id: string): User | undefined {
    this.log("find");
    return this.cache.get(id);
  }
  async load(id: string): Promise<User> {
    const found = this.find(id);
    if (found) return found;
    return new User(id, "x");
  }
  owner(): User {
    return new User("o", "owner");
  }
  static create(): UserRepo {
    return new UserRepo();
  }
}

export const defaultRepo = new UserRepo();

export function makeRepo(): UserRepo {
  return UserRepo.create();
}
